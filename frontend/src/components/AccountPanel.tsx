import { useState } from "react";
import type { AccountData } from "../lib/account";
import type { Signer } from "../lib/hl";
import type { Market, MarketCtx, OpenOrder } from "../lib/types";
import { dateTime, num, pct, px, sigPx, slippagePx, usd } from "../lib/format";
import { describeStatuses, useAction } from "../lib/actions";

type Tab = "positions" | "orders" | "fills" | "history" | "balances";

interface Props {
  acct: AccountData & { refresh: () => Promise<void> };
  markets: Market[];
  ctxs: Record<string, MarketCtx>;
  mids: Record<string, string>;
  signer: Signer | null;
  onSelect: (coin: string) => void;
}

export function AccountPanel({ acct, markets, ctxs, mids, signer, onSelect }: Props) {
  const [tab, setTab] = useState<Tab>("positions");
  const { run, busy } = useAction();
  const byCoin = new Map(markets.map((m) => [m.coin, m]));
  const positions = (acct.ch?.assetPositions ?? []).map((a) => a.position).filter((p) => num(p.szi) !== 0);

  const closePosition = async (coin: string, szi: string) => {
    const m = byCoin.get(coin);
    if (!m || !signer) return;
    const isBuy = num(szi) < 0;
    const ref = num(mids[coin] ?? ctxs[coin]?.markPx);
    const pxs = slippagePx(ref, isBuy, 0.05, m);
    const sz = szi.replace(/^-/, "");
    await run(
      `Close ${coin} ${szi}`,
      () =>
        signer.exchange.order({
          orders: [{ a: m.asset, b: isBuy, p: pxs, s: sz, r: true, t: { limit: { tif: "Ioc" } } }],
          grouping: "na",
        }),
      (r) => describeStatuses(r.response.data.statuses as unknown[]),
    );
    await acct.refresh().catch(() => {});
  };

  const cancel = async (orders: OpenOrder[]) => {
    if (!signer || orders.length === 0) return;
    const cancels = orders.flatMap((o) => {
      const m = byCoin.get(o.coin);
      return m ? [{ a: m.asset, o: o.oid }] : [];
    });
    await run(
      orders.length === 1 ? `Cancel ${orders[0].coin} #${orders[0].oid}` : `Cancel ${orders.length} orders`,
      () => signer.exchange.cancel({ cancels }),
      (r) => `${(r.response.data.statuses as unknown[]).length} cancelled`,
    );
    await acct.refresh().catch(() => {});
  };

  const tabs: [Tab, string, number | null][] = [
    ["positions", "Positions", positions.length],
    ["orders", "Open orders", acct.openOrders.length],
    ["fills", "Fills", null],
    ["history", "Order history", null],
    ["balances", "Spot balances", null],
  ];

  return (
    <div className="panel account" data-testid="account-panel">
      <div className="tabs">
        {tabs.map(([k, l, n]) => (
          <button key={k} className={tab === k ? "active" : ""} onClick={() => setTab(k)} data-testid={`tab-${k}`}>
            {l}
            {n ? <span className="count">{n}</span> : null}
          </button>
        ))}
        <div className="grow" />
        {tab === "orders" && acct.openOrders.length > 0 && (
          <button className="btn btn-small" data-testid="cancel-all" disabled={busy || !signer} onClick={() => cancel(acct.openOrders)}>
            Cancel all
          </button>
        )}
      </div>
      <div className="table-wrap">
        {!signer && <div className="empty">Load a key in Settings to see your account.</div>}
        {signer && tab === "positions" && (
          <table data-testid="positions">
            <thead>
              <tr>
                <th>Coin</th>
                <th>Size</th>
                <th>Entry</th>
                <th>Mark</th>
                <th>Value</th>
                <th>uPnL (ROE)</th>
                <th>Liq. px</th>
                <th>Margin</th>
                <th>Funding</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {positions.length === 0 && (
                <tr>
                  <td colSpan={10} className="empty">
                    No open positions
                  </td>
                </tr>
              )}
              {positions.map((p) => (
                <tr key={p.coin} data-testid={`position-${p.coin}`}>
                  <td>
                    <button className="link" onClick={() => onSelect(p.coin)}>
                      {p.coin}
                    </button>{" "}
                    <span className="dim small">
                      {p.leverage.type} {p.leverage.value}x
                    </span>
                  </td>
                  <td className={`mono ${num(p.szi) >= 0 ? "pos" : "neg"}`}>{p.szi}</td>
                  <td className="mono">{px(p.entryPx)}</td>
                  <td className="mono">{px(ctxs[p.coin]?.markPx)}</td>
                  <td className="mono">{usd(p.positionValue)}</td>
                  <td className={`mono ${num(p.unrealizedPnl) >= 0 ? "pos" : "neg"}`}>
                    {usd(p.unrealizedPnl)} ({pct(p.returnOnEquity, 2)})
                  </td>
                  <td className="mono">{sigPx(p.liquidationPx)}</td>
                  <td className="mono">{usd(p.marginUsed)}</td>
                  <td className="mono">{usd(p.cumFunding?.sinceOpen)}</td>
                  <td>
                    <button
                      className="btn btn-small"
                      data-testid={`close-${p.coin}`}
                      disabled={busy}
                      onClick={() => closePosition(p.coin, p.szi)}
                    >
                      Market close
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        {signer && tab === "orders" && (
          <table data-testid="open-orders">
            <thead>
              <tr>
                <th>Time</th>
                <th>Coin</th>
                <th>Type</th>
                <th>Side</th>
                <th>Price</th>
                <th>Size / orig</th>
                <th>Trigger</th>
                <th>RO</th>
                <th>OID</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {acct.openOrders.length === 0 && (
                <tr>
                  <td colSpan={10} className="empty">
                    No open orders
                  </td>
                </tr>
              )}
              {acct.openOrders.map((o) => (
                <tr key={o.oid} data-testid={`order-${o.oid}`}>
                  <td className="mono dim">{dateTime(o.timestamp)}</td>
                  <td>{o.coin}</td>
                  <td>
                    {o.orderType}
                    {o.tif ? ` ${o.tif}` : ""}
                  </td>
                  <td className={o.side === "B" ? "pos" : "neg"}>{o.side === "B" ? "Buy" : "Sell"}</td>
                  <td className="mono">{px(o.limitPx)}</td>
                  <td className="mono">
                    {o.sz} / {o.origSz}
                  </td>
                  <td className="mono">{o.isTrigger ? o.triggerCondition : "–"}</td>
                  <td>{o.reduceOnly ? "yes" : ""}</td>
                  <td className="mono dim">{o.oid}</td>
                  <td>
                    <button className="btn btn-small" data-testid={`cancel-${o.oid}`} disabled={busy} onClick={() => cancel([o])}>
                      Cancel
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        {signer && tab === "fills" && (
          <table data-testid="fills">
            <thead>
              <tr>
                <th>Time</th>
                <th>Coin</th>
                <th>Direction</th>
                <th>Price</th>
                <th>Size</th>
                <th>Fee</th>
                <th>Closed PnL</th>
                <th>Role</th>
              </tr>
            </thead>
            <tbody>
              {acct.fills.length === 0 && (
                <tr>
                  <td colSpan={8} className="empty">
                    No fills
                  </td>
                </tr>
              )}
              {acct.fills.map((f, i) => (
                <tr key={`${f.tid}-${f.oid}-${i}`}>
                  <td className="mono dim">{dateTime(f.time)}</td>
                  <td>{f.coin}</td>
                  <td className={f.side === "B" ? "pos" : "neg"}>{f.dir}</td>
                  <td className="mono">{px(f.px)}</td>
                  <td className="mono">{f.sz}</td>
                  <td className="mono">
                    {f.fee} {f.feeToken}
                  </td>
                  <td className={`mono ${num(f.closedPnl) >= 0 ? "pos" : "neg"}`}>{usd(f.closedPnl)}</td>
                  <td className="dim">{f.crossed ? "taker" : "maker"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        {signer && tab === "history" && (
          <table data-testid="history">
            <thead>
              <tr>
                <th>Time</th>
                <th>Coin</th>
                <th>Type</th>
                <th>Side</th>
                <th>Price</th>
                <th>Size</th>
                <th>Status</th>
                <th>OID</th>
              </tr>
            </thead>
            <tbody>
              {acct.history.length === 0 && (
                <tr>
                  <td colSpan={8} className="empty">
                    No orders yet
                  </td>
                </tr>
              )}
              {acct.history.map((h, i) => (
                <tr key={`${h.order.oid}-${i}`}>
                  <td className="mono dim">{dateTime(h.statusTimestamp)}</td>
                  <td>{h.order.coin}</td>
                  <td>
                    {h.order.orderType}
                    {h.order.tif ? ` ${h.order.tif}` : ""}
                  </td>
                  <td className={h.order.side === "B" ? "pos" : "neg"}>{h.order.side === "B" ? "Buy" : "Sell"}</td>
                  <td className="mono">{px(h.order.limitPx)}</td>
                  <td className="mono">{h.order.origSz}</td>
                  <td>{h.status}</td>
                  <td className="mono dim">{h.order.oid}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        {signer && tab === "balances" && (
          <table data-testid="balances">
            <thead>
              <tr>
                <th>Token</th>
                <th>Total</th>
                <th>On hold</th>
                <th>Available</th>
                <th>Entry notional</th>
              </tr>
            </thead>
            <tbody>
              {acct.spot.length === 0 && (
                <tr>
                  <td colSpan={5} className="empty">
                    No spot balances
                  </td>
                </tr>
              )}
              {acct.spot.map((b) => (
                <tr key={b.coin}>
                  <td>{b.coin}</td>
                  <td className="mono">{b.total}</td>
                  <td className="mono">{b.hold}</td>
                  <td className="mono">{(num(b.total) - num(b.hold)).toLocaleString("en-US", { maximumFractionDigits: 8 })}</td>
                  <td className="mono">{usd(b.entryNtl)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}

export function MarginSummary({ acct }: { acct: AccountData }) {
  const ch = acct.ch;
  const ms = ch?.marginSummary;
  const rows: [string, string][] = [
    ["Account value", usd(ms?.accountValue)],
    ["Withdrawable", usd(ch?.withdrawable)],
    ["Margin used", usd(ms?.totalMarginUsed)],
    ["Maint. margin", usd(ch?.crossMaintenanceMarginUsed)],
    ["Total position ntl", usd(ms?.totalNtlPos)],
    [
      "Cross leverage",
      ms && num(ms.accountValue) > 0 ? `${(num(ch?.crossMarginSummary.totalNtlPos) / num(ch?.crossMarginSummary.accountValue)).toFixed(2)}x` : "–",
    ],
  ];
  return (
    <div className="panel margin" data-testid="margin-summary">
      <div className="panel-head">
        <span>Perps account</span>
      </div>
      {rows.map(([k, v]) => (
        <div key={k} className="kv">
          <span className="dim">{k}</span>
          <span className="mono">{v}</span>
        </div>
      ))}
    </div>
  );
}
