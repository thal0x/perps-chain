import { useEffect, useMemo, useState } from "react";
import type { Signer } from "../lib/hl";
import type { LeverageSetting, Market, MarketCtx, Position } from "../lib/types";
import { errMsg, num, px, roundPx, roundSz, slippagePx, szStep, usd } from "../lib/format";
import { describeStatuses, useAction } from "../lib/actions";

type Tif = "Gtc" | "Ioc" | "Alo";
type OType = "limit" | "market";

interface Props {
  market: Market | null;
  ctx: MarketCtx | null;
  mid: string | undefined;
  signer: Signer | null;
  priceHint: { px: string; n: number } | null;
  lev: LeverageSetting | null;
  position: Position | null;
  available: { buy: string | undefined; sell: string | undefined };
  onDone: () => void;
  onLevChanged: () => void;
  onOpenSettings: () => void;
}

export function OrderForm(p: Props) {
  const { market: m, signer } = p;
  const [isBuy, setIsBuy] = useState(true);
  const [otype, setOtype] = useState<OType>("limit");
  const [tif, setTif] = useState<Tif>("Gtc");
  const [price, setPrice] = useState("");
  const [size, setSize] = useState("");
  const [reduceOnly, setReduceOnly] = useState(false);
  const [tpsl, setTpsl] = useState(false);
  const [tp, setTp] = useState("");
  const [sl, setSl] = useState("");
  const [slip, setSlip] = useState("5");
  const { run, busy } = useAction();

  // Reset per market; seed price from mid.
  useEffect(() => {
    setPrice("");
    setSize("");
    setTp("");
    setSl("");
    setTpsl(false);
    setReduceOnly(false);
  }, [m?.coin]);
  useEffect(() => {
    if (m && !price && p.mid) {
      try {
        setPrice(roundPx(p.mid, m));
      } catch {
        /* ignore */
      }
    }
  }, [m, p.mid, price]);
  useEffect(() => {
    if (p.priceHint) {
      setPrice(p.priceHint.px);
      setOtype("limit");
    }
  }, [p.priceHint]);

  const ref = num(p.mid ?? p.ctx?.midPx ?? p.ctx?.markPx);
  const slipFrac = Math.min(Math.max(num(slip) / 100 || 0.05, 0.0001), 0.5);

  // Normalised values that will actually be sent.
  const prepared = useMemo(() => {
    if (!m) return { err: "no market" };
    try {
      if (!size) return { err: "enter a size" };
      const s = roundSz(size, m);
      let pxs: string;
      if (otype === "market") {
        if (!Number.isFinite(ref)) return { err: "no reference price for a market order" };
        pxs = slippagePx(ref, isBuy, slipFrac, m);
      } else {
        if (!price) return { err: "enter a price" };
        pxs = roundPx(price, m);
      }
      const tpPx = tpsl && tp ? roundPx(tp, m) : null;
      const slPx = tpsl && sl ? roundPx(sl, m) : null;
      return { s, px: pxs, tpPx, slPx };
    } catch (e) {
      return { err: errMsg(e) };
    }
  }, [m, size, price, otype, ref, isBuy, slipFrac, tpsl, tp, sl]);

  const notional = "s" in prepared && prepared.s ? num(prepared.s) * (otype === "market" ? ref : num(prepared.px)) : NaN;
  const levVal = m?.kind === "perp" ? p.lev?.value ?? Math.min(20, m.maxLeverage) : 1;

  const submit = async () => {
    if (!m || !signer || "err" in prepared) return;
    const { s, px: pxs, tpPx, slPx } = prepared as { s: string; px: string; tpPx: string | null; slPx: string | null };
    const orders: Parameters<Signer["exchange"]["order"]>[0]["orders"] = [
      {
        a: m.asset,
        b: isBuy,
        p: pxs,
        s,
        r: reduceOnly,
        t: { limit: { tif: otype === "market" ? "Ioc" : tif } },
      },
    ];
    // TP/SL children: opposite side, reduce-only, market triggers (limit px with slippage).
    for (const [kind, trig] of [
      ["tp", tpPx],
      ["sl", slPx],
    ] as const) {
      if (!trig) continue;
      orders.push({
        a: m.asset,
        b: !isBuy,
        p: slippagePx(num(trig), !isBuy, slipFrac, m),
        s,
        r: true,
        t: { trigger: { isMarket: true, triggerPx: trig, tpsl: kind } },
      });
    }
    const side = isBuy ? "Buy" : "Sell";
    const r = await run(
      `${side} ${s} ${m.base} ${otype === "market" ? "market" : `@ ${pxs} ${otype === "limit" ? tif : ""}`}`,
      () => signer.exchange.order({ orders, grouping: orders.length > 1 ? "normalTpsl" : "na" }),
      (r) => describeStatuses(r.response.data.statuses as unknown[]),
    );
    if (r) p.onDone();
  };

  const pos = p.position;
  const avail = isBuy ? p.available.buy : p.available.sell;
  const setPctOfMax = (f: number) => {
    if (!m || !Number.isFinite(ref) || ref <= 0) return;
    const a = num(avail);
    if (!Number.isFinite(a)) return;
    const maxSz = m.kind === "perp" ? (a * levVal) / ref : isBuy ? a / ref : a;
    try {
      setSize(roundSz(maxSz * f, m));
    } catch {
      setSize("");
    }
  };

  return (
    <div className="panel orderform" data-testid="order-form">
      {m?.kind === "perp" && (
        <LeverageControl market={m} lev={p.lev} signer={signer} onChanged={p.onLevChanged} />
      )}
      <div className="seg seg-wide side">
        <button className={isBuy ? "active buy" : ""} data-testid="side-buy" onClick={() => setIsBuy(true)}>
          {m?.kind === "spot" ? "Buy" : "Buy / Long"}
        </button>
        <button className={!isBuy ? "active sell" : ""} data-testid="side-sell" onClick={() => setIsBuy(false)}>
          {m?.kind === "spot" ? "Sell" : "Sell / Short"}
        </button>
      </div>
      <div className="seg seg-wide">
        <button className={otype === "limit" ? "active" : ""} data-testid="type-limit" onClick={() => setOtype("limit")}>
          Limit
        </button>
        <button className={otype === "market" ? "active" : ""} data-testid="type-market" onClick={() => setOtype("market")}>
          Market
        </button>
      </div>

      {otype === "limit" ? (
        <label className="field">
          <span>
            Price ({m?.quote ?? "USD"})
            <button className="link" onClick={() => m && p.mid && setPrice(roundPx(p.mid, m))}>
              mid
            </button>
          </span>
          <input data-testid="price" inputMode="decimal" value={price} onChange={(e) => setPrice(e.target.value)} />
        </label>
      ) : (
        <label className="field">
          <span>Max slippage (%)</span>
          <input data-testid="slippage" inputMode="decimal" value={slip} onChange={(e) => setSlip(e.target.value)} />
        </label>
      )}

      <label className="field">
        <span>
          Size ({m?.base ?? ""}) <span className="dim">step {m ? szStep(m) : ""}</span>
        </span>
        <input data-testid="size" inputMode="decimal" value={size} onChange={(e) => setSize(e.target.value)} />
      </label>
      <div className="row gap pcts">
        {[0.1, 0.25, 0.5, 1].map((f) => (
          <button key={f} className="btn btn-small" onClick={() => setPctOfMax(f)} disabled={!avail}>
            {f * 100}%
          </button>
        ))}
      </div>

      {otype === "limit" && (
        <div className="row gap tif">
          {(["Gtc", "Ioc", "Alo"] as const).map((t) => (
            <label key={t} className="radio">
              <input type="radio" name="tif" checked={tif === t} onChange={() => setTif(t)} data-testid={`tif-${t}`} />
              {t}
            </label>
          ))}
        </div>
      )}
      <div className="row gap">
        <label className="check">
          <input type="checkbox" checked={reduceOnly} onChange={(e) => setReduceOnly(e.target.checked)} data-testid="reduce-only" />
          Reduce only
        </label>
        {m?.kind === "perp" && (
          <label className="check">
            <input type="checkbox" checked={tpsl} onChange={(e) => setTpsl(e.target.checked)} data-testid="tpsl" />
            TP / SL
          </label>
        )}
      </div>
      {tpsl && m?.kind === "perp" && (
        <div className="row gap">
          <label className="field">
            <span>TP trigger</span>
            <input data-testid="tp" inputMode="decimal" value={tp} onChange={(e) => setTp(e.target.value)} />
          </label>
          <label className="field">
            <span>SL trigger</span>
            <input data-testid="sl" inputMode="decimal" value={sl} onChange={(e) => setSl(e.target.value)} />
          </label>
        </div>
      )}

      <div className="summary small">
        <div>
          <span className="dim">Sends</span>
          <span className="mono" data-testid="order-preview">
            {"err" in prepared
              ? prepared.err
              : `${prepared.s} @ ${prepared.px}${otype === "market" ? " IOC" : ` ${tif}`}${prepared.tpPx ? ` · TP ${prepared.tpPx}` : ""}${prepared.slPx ? ` · SL ${prepared.slPx}` : ""}`}
          </span>
        </div>
        <div>
          <span className="dim">Notional</span>
          <span className="mono">{usd(notional)}</span>
        </div>
        {m?.kind === "perp" && (
          <div>
            <span className="dim">Margin req.</span>
            <span className="mono">{usd(notional / levVal)}</span>
          </div>
        )}
        <div>
          <span className="dim">{m?.kind === "spot" ? (isBuy ? `Avail. ${m.quote}` : `Avail. ${m.base}`) : "Available"}</span>
          <span className="mono">{m?.kind === "perp" ? usd(avail) : fmtAmt(avail)}</span>
        </div>
        {pos && (
          <div>
            <span className="dim">Position</span>
            <span className={`mono ${num(pos.szi) >= 0 ? "pos" : "neg"}`}>
              {pos.szi} @ {px(pos.entryPx)}
            </span>
          </div>
        )}
      </div>

      {signer ? (
        <button
          className={`btn btn-submit ${isBuy ? "buy" : "sell"}`}
          data-testid="submit-order"
          disabled={busy || "err" in prepared}
          onClick={submit}
        >
          {busy ? "Submitting…" : `${otype === "market" ? "Market " : "Limit "}${isBuy ? "Buy" : "Sell"}`}
        </button>
      ) : (
        <button className="btn btn-submit" onClick={p.onOpenSettings}>
          Load a key to trade
        </button>
      )}
    </div>
  );
}

function fmtAmt(s: string | undefined) {
  const n = num(s);
  return Number.isFinite(n) ? n.toLocaleString("en-US", { maximumFractionDigits: 6 }) : "–";
}

function LeverageControl({
  market,
  lev,
  signer,
  onChanged,
}: {
  market: Market;
  lev: LeverageSetting | null;
  signer: Signer | null;
  onChanged: () => void;
}) {
  const [editing, setEditing] = useState(false);
  const [value, setValue] = useState(lev?.value ?? 20);
  const [cross, setCross] = useState((lev?.type ?? "cross") === "cross");
  const { run, busy } = useAction();
  useEffect(() => {
    if (lev && !editing) {
      setValue(lev.value);
      setCross(lev.type === "cross");
    }
  }, [lev, editing]);

  const apply = async () => {
    if (!signer) return;
    const r = await run(`Leverage ${market.coin} ${cross ? "cross" : "isolated"} ${value}x`, () =>
      signer.exchange.updateLeverage({ asset: market.asset, isCross: cross, leverage: value }),
    );
    if (r) {
      setEditing(false);
      onChanged();
    }
  };

  if (!editing) {
    return (
      <div className="row gap lev-row">
        <button className="btn btn-small" data-testid="leverage-btn" onClick={() => setEditing(true)} disabled={!signer}>
          {lev ? `${lev.type === "cross" ? "Cross" : "Isolated"} ${lev.value}x` : "Leverage"}
        </button>
        <span className="dim small">max {market.maxLeverage}x</span>
      </div>
    );
  }
  return (
    <div className="lev-edit">
      <div className="seg seg-wide">
        <button className={cross ? "active" : ""} disabled={market.onlyIsolated} onClick={() => setCross(true)}>
          Cross
        </button>
        <button className={!cross ? "active" : ""} onClick={() => setCross(false)}>
          Isolated
        </button>
      </div>
      <div className="row gap">
        <input
          type="range"
          min={1}
          max={market.maxLeverage}
          value={value}
          onChange={(e) => setValue(Number(e.target.value))}
          className="grow"
        />
        <input
          className="lev-num mono"
          type="number"
          min={1}
          max={market.maxLeverage}
          value={value}
          data-testid="leverage-value"
          onChange={(e) => setValue(Math.max(1, Math.min(market.maxLeverage, Math.floor(Number(e.target.value) || 1))))}
        />
        <span>x</span>
      </div>
      <div className="row gap end">
        <button className="btn btn-small" onClick={() => setEditing(false)}>
          Cancel
        </button>
        <button className="btn btn-small btn-primary" data-testid="leverage-apply" disabled={busy} onClick={apply}>
          Apply
        </button>
      </div>
    </div>
  );
}
