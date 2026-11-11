// Market maker: keeps a ladder of bids and asks on every perp book and on
// PURR/USDC so that there is always something to trade against.
//
// Every `intervalMs` it reads the oracle price (perps) or its own reference
// price (spot), its position and its open orders, and computes the ladder it
// wants: `levels` bids below and asks above a centre price. The centre leans
// against the position (long -> quotes lower, to sell more and buy less), and
// the total size on each side is capped so that position + resting orders
// never exceed `maxPositionUsd`. A book is only re-quoted when the reference
// moved more than `requoteBps` or a level was (partly) filled; then the
// orders are moved with one batchModify, plus one order / cancel batch for
// levels that appeared or disappeared, across all markets.
import type { InfoClient } from "@nktkas/hyperliquid";
import type { MmConfig } from "./config.ts";
import {
  errText,
  fmtPx,
  fmtSz,
  logger,
  lot,
  num,
  sleep,
  statusesOf,
  usd,
  type Market,
  type Wallet,
} from "./common.ts";

const log = logger("mm");

interface Want {
  isBuy: boolean;
  px: string;
  sz: string;
}
interface Open {
  oid: number;
  coin: string;
  isBuy: boolean;
  px: string;
  sz: string;
}
type WireOrder = {
  a: number;
  b: boolean;
  p: string;
  s: string;
  r: boolean;
  t: { limit: { tif: "Gtc" } };
};

export interface MmStats {
  ticks: number;
  requotes: number;
  errors: number;
  positions: Record<string, number>;
  accountValue: number;
}

export class MarketMaker {
  readonly stats: MmStats = { ticks: 0, requotes: 0, errors: 0, positions: {}, accountValue: 0 };
  private lastCenter = new Map<string, number>();
  private spotRef = new Map<string, number>();
  private lastStatus = 0;
  private lastSweep = 0;
  private startValue = NaN;

  constructor(
    private readonly cfg: MmConfig,
    private readonly info: InfoClient,
    private readonly w: Wallet,
    private readonly markets: Market[],
    private readonly faucetAddress: string | null,
  ) {}

  /** Cross leverage on every quoted perp; a little spot USDC for PURR bids. */
  async setup() {
    for (const m of this.markets.filter((x) => x.kind === "perp")) {
      const lev = Math.max(1, Math.min(this.cfg.leverage, m.maxLeverage));
      try {
        await this.w.exchange.updateLeverage({ asset: m.asset, isCross: true, leverage: lev });
      } catch (e) {
        log.warn(`updateLeverage ${m.coin} ${lev}x: ${errText(e)}`);
      }
    }
    if (this.markets.some((m) => m.kind === "spot")) {
      const s = await this.info.spotClearinghouseState({ user: this.w.address });
      const usdc = num(s.balances.find((b) => b.coin === "USDC")?.total ?? "0");
      if (usdc < this.cfg.spotUsdc / 2) {
        const amt = Math.round(this.cfg.spotUsdc - usdc);
        await this.w.exchange.usdClassTransfer({ amount: String(amt), toPerp: false });
        log.info(`moved ${amt} USDC perp -> spot for PURR/USDC bids`);
      }
    }
    log.info(
      `quoting ${this.markets.map((m) => m.coin).join(", ")} from ${this.w.address}: ${this.cfg.levels} levels, ` +
        `${this.cfg.halfSpreadBps} bp half-spread, ${usd(this.cfg.levelUsd)} first level, cap ${usd(this.cfg.maxPositionUsd)}/market, ${this.cfg.leverage}x`,
    );
  }

  async run(signal: AbortSignal) {
    await this.setup();
    while (!signal.aborted) {
      const t0 = Date.now();
      try {
        await this.tick();
      } catch (e) {
        this.stats.errors++;
        log.warn(`tick failed: ${errText(e)}`);
      }
      await sleep(Math.max(200, this.cfg.intervalMs - (Date.now() - t0)), signal);
    }
  }

  private async tick() {
    this.stats.ticks++;
    const user = this.w.address;
    const hasSpot = this.markets.some((m) => m.kind === "spot");
    const [perpCtx, spotCtx, ch, spot, oo] = await Promise.all([
      this.info.metaAndAssetCtxs(),
      hasSpot ? this.info.spotMetaAndAssetCtxs() : Promise.resolve(null),
      this.info.clearinghouseState({ user }),
      hasSpot ? this.info.spotClearinghouseState({ user }) : Promise.resolve(null),
      this.info.openOrders({ user }),
    ]);
    const oracle = new Map<string, number>();
    perpCtx[0].universe.forEach((u, i) => oracle.set(u.name, num(perpCtx[1][i]?.oraclePx)));
    const spotMid = new Map<string, number>();
    if (spotCtx) {
      for (const c of spotCtx[1]) spotMid.set(c.coin, num(c.midPx ?? c.markPx));
      // Pairs are keyed "@index" in ctxs except canonical names; map PURR/USDC.
      for (const p of spotCtx[0].universe) {
        const c = spotCtx[1].find((x) => x.coin === p.name || x.coin === `@${p.index}`);
        if (c) spotMid.set(p.name, num(c.midPx ?? c.markPx));
      }
    }
    const pos = new Map<string, number>();
    for (const ap of ch.assetPositions) pos.set(ap.position.coin, num(ap.position.szi));
    this.stats.accountValue = num(ch.marginSummary.accountValue);
    if (Number.isNaN(this.startValue)) this.startValue = this.stats.accountValue;
    const bal = (coin: string) => num(spot?.balances.find((b) => b.coin === coin)?.total ?? "0");

    const open = new Map<string, Open[]>();
    for (const o of oo) {
      const l = open.get(o.coin) ?? [];
      l.push({ oid: o.oid, coin: o.coin, isBuy: o.side === "B", px: o.limitPx, sz: o.sz });
      open.set(o.coin, l);
    }

    const modifies: { oid: number; order: WireOrder }[] = [];
    const places: WireOrder[] = [];
    const cancels: { a: number; o: number }[] = [];
    const touched: string[] = [];
    const c = this.cfg;

    for (const m of this.markets) {
      let ref: number;
      let inv: number; // signed inventory in base units, relative to target
      let bidCapUsd: number;
      let askCapUsd: number;
      if (m.kind === "perp") {
        ref = oracle.get(m.coin) ?? NaN;
        inv = pos.get(m.coin) ?? 0;
        const invUsd = inv * ref;
        bidCapUsd = Math.max(0, c.maxPositionUsd - invUsd);
        askCapUsd = Math.max(0, c.maxPositionUsd + invUsd);
      } else {
        const base = m.coin.split("/")[0];
        const prev = this.spotRef.get(m.coin);
        ref = prev ?? (Number.isFinite(spotMid.get(m.coin)) ? spotMid.get(m.coin)! : 1);
        inv = bal(base) - c.spotTargetPurr;
        const maxDev = c.spotMaxDeviationPurr;
        bidCapUsd = Math.min(Math.max(0, maxDev - inv) * ref, bal("USDC") * 0.95);
        askCapUsd = Math.max(0, Math.min(maxDev + inv, bal(base))) * ref;
      }
      if (!(ref > 0)) continue;
      this.stats.positions[m.coin] = inv;
      const capBase = m.kind === "perp" ? c.maxPositionUsd / ref : c.spotMaxDeviationPurr;
      const frac = Math.max(-1, Math.min(1, inv / capBase));
      const center = ref * (1 - (frac * c.maxSkewBps) / 1e4);
      if (m.kind === "spot") {
        // No oracle for spot: the reference drifts toward where inventory pushes
        // the quotes, so persistent buying lifts the price and selling lowers it.
        this.spotRef.set(m.coin, ref + 0.03 * (center - ref));
      }

      const want: Want[] = [...this.ladder(m, center, true, bidCapUsd), ...this.ladder(m, center, false, askCapUsd)];
      const cur = open.get(m.coin) ?? [];
      const last = this.lastCenter.get(m.coin);
      const moved = last === undefined || Math.abs(center / last - 1) * 1e4 >= c.requoteBps;
      const shape =
        cur.filter((o) => o.isBuy).length !== want.filter((x) => x.isBuy).length ||
        cur.filter((o) => !o.isBuy).length !== want.filter((x) => !x.isBuy).length;
      const partial = this.pairs(cur, want).some(([o, w]) => o && w && num(o.sz) < 0.5 * num(w.sz));
      if (!moved && !shape && !partial) continue;

      // Move the side away from the price first so we never cross our own quotes.
      const up = last !== undefined && center > last;
      const sides = up ? [false, true] : [true, false];
      let n = 0;
      for (const side of sides) {
        for (const [o, w] of this.pairs(cur.filter((x) => x.isBuy === side), want.filter((x) => x.isBuy === side))) {
          if (o && w) {
            if (num(o.px) === num(w.px) && num(o.sz) === num(w.sz)) continue;
            modifies.push({ oid: o.oid, order: this.wire(m, w) });
            n++;
          } else if (w) {
            places.push(this.wire(m, w));
            n++;
          } else if (o) {
            cancels.push({ a: m.asset, o: o.oid });
            n++;
          }
        }
      }
      this.lastCenter.set(m.coin, center);
      if (n) touched.push(m.coin);
    }

    if (!modifies.length && !places.length && !cancels.length) return this.maybeStatus();
    this.stats.requotes++;
    const errs: string[] = [];
    if (cancels.length) {
      try {
        await this.w.exchange.cancel({ cancels });
      } catch (e) {
        errs.push(`cancel: ${errText(e)}`);
      }
    }
    if (modifies.length) {
      try {
        await this.w.exchange.batchModify({ modifies });
      } catch (e) {
        const bad = statusesOf(e).filter((s) => typeof s === "object" && s && "error" in s).length;
        errs.push(`modify: ${bad || "?"}/${modifies.length} failed (${firstErr(e)})`);
      }
    }
    if (places.length) {
      try {
        await this.w.exchange.order({ orders: places, grouping: "na" });
      } catch (e) {
        const bad = statusesOf(e).filter((s) => typeof s === "object" && s && "error" in s).length;
        errs.push(`place: ${bad || "?"}/${places.length} failed (${firstErr(e)})`);
      }
    }
    if (errs.length) {
      this.stats.errors++;
      // Forget the centres so the next tick rebuilds from the actual open orders.
      for (const coin of touched) this.lastCenter.delete(coin);
    }
    log.info(
      `requote ${touched.join(" ")}: ${modifies.length} moved, ${places.length} new, ${cancels.length} cancelled` +
        (errs.length ? ` · ${errs.join("; ")}` : ""),
    );
    this.maybeStatus();
  }

  /** Pair current orders with wanted ones, best price first. */
  private pairs(cur: Open[], want: Want[]): [Open | undefined, Want | undefined][] {
    const out: [Open | undefined, Want | undefined][] = [];
    for (const side of [true, false]) {
      const o = cur.filter((x) => x.isBuy === side).sort((a, b) => (side ? num(b.px) - num(a.px) : num(a.px) - num(b.px)));
      const w = want.filter((x) => x.isBuy === side);
      for (let i = 0; i < Math.max(o.length, w.length); i++) out.push([o[i], w[i]]);
    }
    return out;
  }

  /** Levels for one side, best first, within `capUsd` total. */
  private ladder(m: Market, center: number, isBuy: boolean, capUsd: number): Want[] {
    const c = this.cfg;
    const out: Want[] = [];
    let left = capUsd;
    let lastPx = "";
    for (let i = 0; i < c.levels && left >= 11; i++) {
      const off = (c.halfSpreadBps + i * c.levelStepBps) / 1e4;
      // fmtPx rounds down by at most one tick (< 1 bp here), well inside the spread.
      const px = fmtPx(center * (isBuy ? 1 - off : 1 + off), m);
      if (px === lastPx) continue;
      const usdWanted = Math.min(left, c.levelUsd * c.levelSizeGrowth ** i);
      let sz = fmtSz(usdWanted / num(px), m);
      if (!sz || num(sz) * num(px) < 11) {
        // The smallest order the chain accepts is $10 notional.
        const minSz = fmtSz(Math.max(lot(m), 11 / num(px)) * 1.01 + lot(m), m);
        if (!minSz || num(minSz) * num(px) > left) break;
        sz = minSz;
      }
      out.push({ isBuy, px, sz });
      left -= num(sz) * num(px);
      lastPx = px;
    }
    return out;
  }

  private wire(m: Market, w: Want): WireOrder {
    return { a: m.asset, b: w.isBuy, p: w.px, s: w.sz, r: false, t: { limit: { tif: "Gtc" } } };
  }

  private maybeStatus() {
    const now = Date.now();
    if (now - this.lastStatus < 60_000) return;
    this.lastStatus = now;
    const p = Object.entries(this.stats.positions)
      .map(([k, v]) => `${k} ${v >= 0 ? "+" : ""}${+v.toFixed(5)}`)
      .join(", ");
    const pnl = this.stats.accountValue - this.startValue;
    log.info(`status: perp equity ${usd(this.stats.accountValue)} (${pnl >= 0 ? "+" : ""}${usd(pnl)} since start) · inventory ${p}`);
    void this.maybeSweep();
  }

  /** Optional: send perp equity above `sweepAboveUsd` back to the faucet. */
  private async maybeSweep() {
    const c = this.cfg;
    if (!c.sweepAboveUsd || !this.faucetAddress || Date.now() - this.lastSweep < 10 * 60_000) return;
    const excess = Math.floor(this.stats.accountValue - c.sweepAboveUsd);
    if (excess < 100) return;
    this.lastSweep = Date.now();
    try {
      const ch = await this.info.clearinghouseState({ user: this.w.address });
      const amt = Math.min(excess, Math.floor(num(ch.withdrawable) * 0.5));
      if (amt < 100) return;
      await this.w.exchange.usdSend({ destination: this.faucetAddress as `0x${string}`, amount: String(amt) });
      log.info(`swept ${amt} USDC profit back to the faucet`);
    } catch (e) {
      log.warn(`sweep failed: ${errText(e)}`);
    }
  }

  /** Cancel every open order of the MM (graceful shutdown). */
  async cancelAll() {
    try {
      const oo = await this.info.openOrders({ user: this.w.address });
      if (!oo.length) return log.info("shutdown: no open orders");
      const byCoin = new Map(this.markets.map((m) => [m.coin, m.asset]));
      const cancels = oo.flatMap((o) => (byCoin.has(o.coin) ? [{ a: byCoin.get(o.coin)!, o: o.oid }] : []));
      await this.w.exchange.cancel({ cancels });
      log.info(`shutdown: cancelled ${cancels.length} open orders`);
    } catch (e) {
      log.warn(`shutdown cancel failed: ${errText(e)}`);
    }
  }
}

function firstErr(e: unknown): string {
  const s = statusesOf(e).find((x) => typeof x === "object" && x && "error" in x) as { error?: string } | undefined;
  return s?.error ?? errText(e);
}
