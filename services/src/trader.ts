// Trader bots: wallets with different habits that trade against the market
// maker (and each other) at a human-ish pace, so books move and fills,
// candles and liquidations show up in the UI.
//
//   bull      leans to buying, mostly market orders
//   bear      leans to selling, mostly market orders
//   reverter  buys dips and sells rips relative to a moving average, often
//             with resting limit orders that it cancels if they go stale
//   degen     small isolated stakes at max leverage on BTC/ETH, never
//             cuts a loss, takes small profits: gets liquidated now and then
//
// Every wallet asks the faucet for USDC at start and whenever its
// withdrawable balance drops below `topUpBelowUsd`.
import type { InfoClient } from "@nktkas/hyperliquid";
import type { TraderConfig } from "./config.ts";
import { requestDrip } from "./faucet.ts";
import {
  chance,
  errText,
  fmtPx,
  fmtSz,
  logger,
  num,
  pick,
  rand,
  sleep,
  statusesOf,
  usd,
  type Logger,
  type Market,
  type Wallet,
} from "./common.ts";

export type Personality = "bull" | "bear" | "reverter" | "degen";
export const PERSONALITIES: Personality[] = ["bull", "bear", "reverter", "degen"];

interface Style {
  buyProb: number; // when not mean-reverting
  marketProb: number; // market (IOC) vs resting limit
  leverage: number; // target leverage (capped per market)
  sizeFrac: [number, number]; // order notional as a fraction of equity x leverage
  closeProb: number;
  maxUse: number; // stop adding when position notional > equity x leverage x maxUse
  limitAwayBps: [number, number];
  staleMs: number;
}

const STYLES: Record<Personality, Style> = {
  bull: { buyProb: 0.68, marketProb: 0.7, leverage: 5, sizeFrac: [0.04, 0.15], closeProb: 0.07, maxUse: 0.6, limitAwayBps: [2, 25], staleMs: 90_000 },
  bear: { buyProb: 0.32, marketProb: 0.7, leverage: 5, sizeFrac: [0.04, 0.15], closeProb: 0.07, maxUse: 0.6, limitAwayBps: [2, 25], staleMs: 90_000 },
  reverter: { buyProb: 0.5, marketProb: 0.3, leverage: 3, sizeFrac: [0.05, 0.2], closeProb: 0.1, maxUse: 0.6, limitAwayBps: [3, 30], staleMs: 75_000 },
  degen: { buyProb: 0.5, marketProb: 1, leverage: 50, sizeFrac: [1, 1], closeProb: 0, maxUse: 1, limitAwayBps: [0, 0], staleMs: 60_000 },
};

interface OpenOrd {
  coin: string;
  oid: number;
  timestamp: number;
}

export class Trader {
  readonly log: Logger;
  readonly style: Style;
  stats = { actions: 0, fills: 0, topUps: 0, liquidations: 0, errors: 0 };
  private nextDripAt = 0;
  private levSet = new Set<string>();
  private ema = new Map<string, number>();
  private ledgerSince = Date.now();
  private spotReady = false;

  constructor(
    readonly name: string,
    readonly kind: Personality,
    private readonly cfg: TraderConfig,
    private readonly info: InfoClient,
    private readonly w: Wallet,
    private readonly markets: Market[],
    private readonly faucetUrl: string,
    private readonly faucetToken: string | null,
  ) {
    this.log = logger(name, "traders");
    this.style = STYLES[kind];
  }

  private get perps() {
    const p = this.markets.filter((m) => m.kind === "perp");
    if (this.kind === "degen") {
      // Highest-leverage markets (BTC, ETH at 50x): liquidation is ~1% away.
      const max = Math.max(...p.map((m) => m.maxLeverage));
      return p.filter((m) => m.maxLeverage === max);
    }
    return p;
  }

  async run(signal: AbortSignal) {
    this.log.info(`${this.kind} trader ${this.w.address}`);
    await this.ensureFunds(true);
    // Stagger start so the wallets don't move in lockstep.
    await sleep(rand(0, 5000) / this.cfg.activity, signal);
    while (!signal.aborted) {
      try {
        await this.step();
      } catch (e) {
        this.stats.errors++;
        this.log.warn(`step failed: ${errText(e)}`);
      }
      await sleep((rand(5_000, 20_000) / this.cfg.activity), signal);
    }
  }

  /** Top up from the faucet when withdrawable USDC is below the threshold. */
  private async ensureFunds(startup = false): Promise<number> {
    const ch = await this.info.clearinghouseState({ user: this.w.address });
    const wd = num(ch.withdrawable);
    if (wd >= this.cfg.topUpBelowUsd || Date.now() < this.nextDripAt) return wd;
    const r = await requestDrip(this.faucetUrl, this.w.address, this.faucetToken);
    if (r.ok) {
      this.stats.topUps++;
      this.nextDripAt = Date.now() + 30_000;
      this.log.info(`${startup ? "funded" : "topped up"} by faucet: +${r.usdc} USDC${r.purr ? ` +${r.purr} PURR` : ""} (withdrawable was ${usd(wd)})`);
      return wd + r.usdc;
    }
    this.nextDripAt = Date.now() + Math.max(30, r.retryAfterSec ?? 60) * 1000;
    this.log.info(`faucet top-up refused (withdrawable ${usd(wd)}): ${r.error}`);
    return wd;
  }

  private async step() {
    this.stats.actions++;
    const user = this.w.address;
    await this.checkLiquidations();
    await this.ensureFunds();
    const [ch, oo, mids] = await Promise.all([
      this.info.clearinghouseState({ user }),
      this.info.frontendOpenOrders({ user }),
      this.info.allMids(),
    ]);
    const equity = num(ch.marginSummary.accountValue);
    const withdrawable = num(ch.withdrawable);
    for (const m of this.markets) {
      const px = num(mids[m.coin]);
      if (px > 0) this.ema.set(m.coin, (this.ema.get(m.coin) ?? px) * 0.8 + px * 0.2);
    }
    const positions = ch.assetPositions.map((a) => a.position).filter((p) => num(p.szi) !== 0);

    // Housekeeping: cancel resting orders that have gone stale.
    const stale = (oo as OpenOrd[]).filter((o) => Date.now() - o.timestamp > this.style.staleMs);
    if (stale.length) {
      const byCoin = new Map(this.markets.map((m) => [m.coin, m.asset]));
      const cancels = stale.flatMap((o) => (byCoin.has(o.coin) ? [{ a: byCoin.get(o.coin)!, o: o.oid }] : []));
      try {
        await this.w.exchange.cancel({ cancels });
        this.log.info(`cancel ${stale.length} stale order(s): ${[...new Set(stale.map((o) => o.coin))].join(" ")}`);
      } catch (e) {
        this.log.info(`cancel stale: ${errText(e)}`);
      }
    }

    if (equity < 15) return; // waiting for the faucet

    if (this.kind === "degen") return this.degenStep(positions, equity, withdrawable, mids);

    // Spot now and then.
    if (chance(this.cfg.spotShare)) {
      const spotM = this.markets.find((m) => m.kind === "spot");
      if (spotM) return this.spotTrade(spotM, num(mids[spotM.coin]), withdrawable);
    }

    // Close a position now and then (more likely the bigger it is).
    if (positions.length) {
      const big = positions.find((p) => Math.abs(num(p.positionValue)) > equity * this.style.leverage * this.style.maxUse);
      if (big || chance(this.style.closeProb)) {
        const p = big ?? pick(positions);
        const m = this.perps.find((x) => x.coin === p.coin);
        if (m) {
          const part = big ? 0.5 : chance(0.5) ? 1 : rand(0.3, 0.7);
          return this.closePosition(m, num(p.szi), num(mids[m.coin]), part);
        }
      }
    }

    const m = pick(this.perps);
    const mid = num(mids[m.coin]);
    if (!(mid > 0)) return;
    let isBuy: boolean;
    if (this.kind === "reverter") {
      const ema = this.ema.get(m.coin) ?? mid;
      isBuy = mid < ema ? chance(0.8) : mid > ema ? chance(0.2) : chance(0.5);
    } else isBuy = chance(this.style.buyProb);
    const lev = await this.leverage(m);
    const pos = positions.find((p) => p.coin === m.coin);
    const posNtl = pos ? num(pos.szi) * mid : 0;
    const adding = (isBuy && posNtl >= 0) || (!isBuy && posNtl <= 0);
    if (adding && Math.abs(posNtl) > equity * lev * this.style.maxUse) isBuy = !isBuy;
    let ntl = equity * lev * rand(...this.style.sizeFrac);
    if (adding) ntl = Math.min(ntl, withdrawable * lev * 0.8);
    if (ntl < 12) return;
    const market = chance(this.style.marketProb);
    await this.place(m, isBuy, ntl / mid, mid, market);
  }

  /**
   * The degen bets a small stake (5% of equity) per trade on an isolated
   * position at the market's maximum leverage, so a ~1% move against it
   * (half the initial margin at 50x) liquidates the position, while the rest
   * of the account survives to try again.
   */
  private async degenStep(positions: { coin: string; szi: string; returnOnEquity: string }[], equity: number, withdrawable: number, mids: Record<string, string>) {
    const stake = Math.max(30, equity * 0.05);
    const p = positions[0];
    if (p) {
      const m = this.perps.find((x) => x.coin === p.coin);
      if (!m) return;
      const roe = num(p.returnOnEquity);
      if (roe >= 0.15) {
        this.log.info(`${p.coin} up ${(roe * 100).toFixed(0)}% on margin, taking profit`);
        return this.closePosition(m, num(p.szi), num(mids[m.coin]), 1);
      }
      // Losing? Double down once in a while (that's what makes it a degen).
      if (roe < -0.15 && withdrawable > stake * 2 && chance(0.3)) {
        const mid = num(mids[m.coin]);
        const lev = await this.leverage(m);
        this.log.info(`${p.coin} down ${(-roe * 100).toFixed(0)}% on margin, doubling down`);
        return this.place(m, num(p.szi) > 0, (stake * lev * 0.95) / mid, mid, true);
      }
      return;
    }
    const m = pick(this.perps);
    const mid = num(mids[m.coin]);
    if (!(mid > 0) || withdrawable < stake * 1.1) return;
    const lev = await this.leverage(m);
    this.log.info(`aping into ${m.coin}: ${usd(stake)} isolated margin at ${lev}x`);
    await this.place(m, chance(0.5), (stake * lev * 0.95) / mid, mid, true);
  }

  private async leverage(m: Market): Promise<number> {
    const lev = Math.max(1, Math.min(this.style.leverage, m.maxLeverage));
    if (!this.levSet.has(m.coin)) {
      const isCross = this.kind !== "degen";
      try {
        await this.w.exchange.updateLeverage({ asset: m.asset, isCross, leverage: lev });
        this.levSet.add(m.coin);
      } catch (e) {
        this.log.warn(`updateLeverage ${m.coin}: ${errText(e)}`);
      }
    }
    return lev;
  }

  private async place(m: Market, isBuy: boolean, size: number, mid: number, market: boolean, reduceOnly = false) {
    const sz = fmtSz(size, m);
    if (!sz) return;
    let pxNum: number;
    let tif: "Ioc" | "Gtc";
    if (market) {
      pxNum = mid * (isBuy ? 1.01 : 0.99); // 1% slippage limit, as the SDK examples do
      tif = "Ioc";
    } else {
      const away = rand(...this.style.limitAwayBps) / 1e4;
      pxNum = mid * (isBuy ? 1 - away : 1 + away);
      tif = "Gtc";
    }
    const p = fmtPx(pxNum, m);
    if (num(p) * num(sz) < 10.5 && !reduceOnly) return;
    const what = `${market ? "market" : "limit"} ${isBuy ? "buy" : "sell"} ${sz} ${m.coin}${market ? "" : ` @ ${p}`}${reduceOnly ? " (close)" : ""}`;
    try {
      const r = await this.w.exchange.order({
        orders: [{ a: m.asset, b: isBuy, p, s: sz, r: reduceOnly, t: { limit: { tif } } }],
        grouping: "na",
      });
      const st = r.response.data.statuses[0] as Record<string, { totalSz?: string; avgPx?: string; oid?: number }>;
      if (st?.filled) {
        this.stats.fills++;
        this.log.info(`${what}: filled ${st.filled.totalSz} @ ${st.filled.avgPx}`);
      } else if (st?.resting) this.log.info(`${what}: resting (oid ${st.resting.oid})`);
      else this.log.info(`${what}: ${JSON.stringify(st)}`);
    } catch (e) {
      const s = statusesOf(e)[0] as { error?: string } | undefined;
      this.log.info(`${what}: rejected: ${s?.error ?? errText(e)}`);
    }
  }

  private async closePosition(m: Market, szi: number, mid: number, part: number) {
    if (!(mid > 0)) return;
    let size = Math.abs(szi) * part;
    if (size * mid < 11) size = Math.abs(szi);
    return this.place(m, szi < 0, size, mid, true, true);
  }

  private async spotTrade(m: Market, mid: number, perpWithdrawable: number) {
    if (!(mid > 0)) return;
    const base = m.coin.split("/")[0];
    const s = await this.info.spotClearinghouseState({ user: this.w.address });
    const bal = (c: string) => {
      const b = s.balances.find((x) => x.coin === c);
      return b ? num(b.total) - num(b.hold) : 0;
    };
    let usdc = bal("USDC");
    if (!this.spotReady && usdc < 200 && perpWithdrawable > 2000) {
      const amt = Math.round(Math.min(2000, perpWithdrawable * 0.15));
      try {
        await this.w.exchange.usdClassTransfer({ amount: String(amt), toPerp: false });
        usdc += amt;
        this.log.info(`moved ${amt} USDC perp -> spot`);
      } catch (e) {
        this.log.warn(`usdClassTransfer: ${errText(e)}`);
      }
    }
    this.spotReady = true;
    const held = bal(base);
    let isBuy = this.kind === "bull" ? chance(0.65) : this.kind === "bear" ? chance(0.35) : chance(0.5);
    if (this.kind === "reverter") {
      const ema = this.ema.get(m.coin) ?? mid;
      isBuy = mid <= ema ? chance(0.75) : chance(0.25);
    }
    if (isBuy && usdc < 15) isBuy = false;
    if (!isBuy && held * mid < 12) isBuy = true;
    const ntl = isBuy ? Math.min(usdc * 0.9, rand(20, 250)) : Math.min(held * mid, rand(20, 250));
    if (ntl < 12) return;
    await this.place(m, isBuy, ntl / mid, mid, chance(this.style.marketProb));
  }

  private async checkLiquidations() {
    try {
      const since = this.ledgerSince;
      const ups = await this.info.userNonFundingLedgerUpdates({ user: this.w.address, startTime: since });
      for (const u of ups) {
        this.ledgerSince = Math.max(this.ledgerSince, u.time + 1);
        if (u.delta.type === "liquidation") {
          this.stats.liquidations++;
          const d = u.delta as { liquidatedNtlPos?: string; accountValue?: string };
          this.log.info(`LIQUIDATED: ${usd(num(d.liquidatedNtlPos))} of positions closed by the exchange (account value was ${usd(num(d.accountValue))})`);
        }
      }
    } catch (e) {
      this.log.warn(`ledger check: ${errText(e)}`);
    }
  }

  /** Cancel resting orders (shutdown). */
  async cancelAll() {
    try {
      const oo = await this.info.openOrders({ user: this.w.address });
      if (!oo.length) return;
      const byCoin = new Map(this.markets.map((m) => [m.coin, m.asset]));
      await this.w.exchange.cancel({ cancels: oo.flatMap((o) => (byCoin.has(o.coin) ? [{ a: byCoin.get(o.coin)!, o: o.oid }] : [])) });
      this.log.info(`shutdown: cancelled ${oo.length} open orders`);
    } catch (e) {
      this.log.warn(`shutdown cancel: ${errText(e)}`);
    }
  }
}
