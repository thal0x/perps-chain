import type { LiveBook } from "../lib/market";
import type { Level, Market } from "../lib/types";
import { num, px } from "../lib/format";

const DEPTH = 11;

export function OrderBook({ live, market, onPrice }: { live: LiveBook; market: Market | null; onPrice: (p: string) => void }) {
  const book = live.book;
  const bids = book?.levels[0].slice(0, DEPTH) ?? [];
  const asks = book?.levels[1].slice(0, DEPTH) ?? [];
  const cum = (ls: Level[]) => {
    let t = 0;
    return ls.map((l) => (t += num(l.sz)));
  };
  const bc = cum(bids);
  const ac = cum(asks);
  const max = Math.max(bc[bc.length - 1] ?? 0, ac[ac.length - 1] ?? 0) || 1;
  const bb = num(bids[0]?.px);
  const ba = num(asks[0]?.px);
  const spread = Number.isFinite(bb) && Number.isFinite(ba) ? ba - bb : NaN;

  const row = (l: Level, c: number, side: "bid" | "ask", i: number) => (
    <div key={`${side}${i}`} className={`book-row ${side}`} onClick={() => onPrice(l.px)} title="Click to use this price">
      <div className="depth" style={{ width: `${(c / max) * 100}%` }} />
      <span className="mono">{px(l.px)}</span>
      <span className="mono">{l.sz}</span>
      <span className="mono dim">{c.toFixed(Math.min(market?.szDecimals ?? 2, 5))}</span>
    </div>
  );

  return (
    <div className="panel book" data-testid="orderbook">
      <div className="panel-head">
        <span>Order book</span>
        <span className="dim small">{live.source ? `via ${live.source.toUpperCase()}` : "loading…"}</span>
      </div>
      <div className="book-cols dim small">
        <span>Price</span>
        <span>Size{market ? ` (${market.base})` : ""}</span>
        <span>Total</span>
      </div>
      <div className="book-side asks" data-testid="book-asks">
        {[...asks.map((l, i) => row(l, ac[i], "ask", i))].reverse()}
      </div>
      <div className="spread mono">
        {Number.isFinite(spread) ? (
          <>
            Spread {spread.toFixed(Math.max(0, decimals(asks[0]?.px)))}{" "}
            <span className="dim">({((spread / ((ba + bb) / 2)) * 100).toFixed(3)}%)</span>
          </>
        ) : book ? (
          "one-sided book"
        ) : (
          "–"
        )}
      </div>
      <div className="book-side bids" data-testid="book-bids">
        {bids.map((l, i) => row(l, bc[i], "bid", i))}
      </div>
    </div>
  );
}

function decimals(s?: string) {
  if (!s || !s.includes(".")) return 0;
  const f = s.split(".")[1];
  return f === "0" ? 0 : f.length;
}
