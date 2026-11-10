import type { Trade } from "../lib/types";
import { px, time } from "../lib/format";

export function Trades({ trades }: { trades: Trade[] }) {
  return (
    <div className="panel trades" data-testid="trades">
      <div className="panel-head">
        <span>Recent trades</span>
      </div>
      <div className="book-cols dim small">
        <span>Price</span>
        <span>Size</span>
        <span>Time</span>
      </div>
      <div className="trades-list">
        {trades.length === 0 && <div className="empty">No trades yet</div>}
        {trades.map((t) => (
          <div key={`${t.tid}-${t.time}-${t.side}`} className="trade-row">
            <span className={`mono ${t.side === "B" ? "pos" : "neg"}`}>{px(t.px)}</span>
            <span className="mono">{t.sz}</span>
            <span className="mono dim">{time(t.time)}</span>
          </div>
        ))}
      </div>
    </div>
  );
}
