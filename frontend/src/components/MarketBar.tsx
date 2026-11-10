import { useEffect, useRef, useState } from "react";
import type { Market, MarketCtx } from "../lib/types";
import { num, pct, px, usd } from "../lib/format";

interface Props {
  markets: Market[];
  selected: Market | null;
  ctx: MarketCtx | null;
  mids: Record<string, string>;
  onSelect: (coin: string) => void;
}

export function MarketBar({ markets, selected, ctx, mids, onSelect }: Props) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const h = (e: MouseEvent) => ref.current && !ref.current.contains(e.target as Node) && setOpen(false);
    document.addEventListener("mousedown", h);
    return () => document.removeEventListener("mousedown", h);
  }, []);

  const mid = selected ? mids[selected.coin] ?? ctx?.midPx ?? undefined : undefined;
  const prev = num(ctx?.prevDayPx);
  const mark = num(ctx?.markPx);
  const chg = Number.isFinite(prev) && prev > 0 && Number.isFinite(mark) ? mark / prev - 1 : NaN;

  return (
    <div className="marketbar">
      <div className="market-picker" ref={ref}>
        <button className="market-btn" data-testid="market-picker" onClick={() => setOpen((o) => !o)}>
          <span className="market-name">{selected ? label(selected) : "…"}</span>
          {selected && <span className={`tag tag-${selected.kind}`}>{selected.kind === "perp" ? `${selected.maxLeverage}x` : "SPOT"}</span>}
          <span className="caret">▾</span>
        </button>
        {open && (
          <div className="market-menu">
            {(["perp", "spot"] as const).map((k) => (
              <div key={k}>
                <div className="menu-section">{k === "perp" ? "Perpetuals" : "Spot"}</div>
                {markets
                  .filter((m) => m.kind === k)
                  .map((m) => (
                    <button
                      key={m.coin}
                      className={`menu-item ${m.coin === selected?.coin ? "active" : ""}`}
                      data-testid={`market-${m.coin}`}
                      onClick={() => {
                        onSelect(m.coin);
                        setOpen(false);
                      }}
                    >
                      <span>{label(m)}</span>
                      <span className="mono dim">{px(mids[m.coin])}</span>
                    </button>
                  ))}
              </div>
            ))}
          </div>
        )}
      </div>

      <div className="stats">
        <Stat k="Mid" v={px(mid)} testid="stat-mid" />
        <Stat k="Mark" v={px(ctx?.markPx)} />
        {selected?.kind === "perp" && <Stat k="Oracle" v={px(ctx?.oraclePx)} />}
        <Stat k="24h change" v={Number.isFinite(chg) ? `${chg >= 0 ? "+" : ""}${(chg * 100).toFixed(2)}%` : "–"} cls={chg >= 0 ? "pos" : "neg"} />
        <Stat k="24h volume" v={usd(ctx?.dayNtlVlm)} />
        {selected?.kind === "perp" && (
          <>
            <Stat
              k="Open interest"
              v={Number.isFinite(num(ctx?.openInterest)) ? `${ctx?.openInterest} ${selected.coin}` : "–"}
            />
            <Stat k="Funding (1h)" v={pct(ctx?.funding, 4)} cls={num(ctx?.funding) >= 0 ? "pos" : "neg"} />
          </>
        )}
      </div>
    </div>
  );
}

function Stat({ k, v, cls, testid }: { k: string; v: string; cls?: string; testid?: string }) {
  return (
    <div className="stat">
      <div className="stat-k">{k}</div>
      <div className={`stat-v mono ${cls ?? ""}`} data-testid={testid}>
        {v}
      </div>
    </div>
  );
}

export function label(m: Market) {
  return m.kind === "perp" ? `${m.coin}-USD` : m.coin;
}
