import { useCallback, useEffect, useRef, useState } from "react";
import type { Clients } from "./hl";
import type { Book, Candle, Market, MarketCtx, Trade } from "./types";
import { usePoll, useSub } from "./useSub";
import { errMsg } from "./format";

const SPOT_OFFSET = 10_000;

export interface Markets {
  markets: Market[];
  ctxs: Record<string, MarketCtx>;
  mids: Record<string, string>;
  error: string | null;
  loaded: boolean;
}

/** Perps from metaAndAssetCtxs, spot from spotMetaAndAssetCtxs; mids live via WS allMids. */
export function useMarkets(c: Clients): Markets {
  const [state, setState] = useState<Markets>({ markets: [], ctxs: {}, mids: {}, error: null, loaded: false });

  const load = useCallback(async () => {
    try {
      const [perp, spot, mids] = await Promise.all([
        c.info.metaAndAssetCtxs(),
        c.info.spotMetaAndAssetCtxs(),
        c.info.allMids(),
      ]);
      const markets: Market[] = [];
      const ctxs: Record<string, MarketCtx> = {};
      const [meta, actxs] = perp as unknown as [
        { universe: { name: string; szDecimals: number; maxLeverage: number; onlyIsolated?: boolean; isDelisted?: boolean }[] },
        MarketCtx[],
      ];
      meta.universe.forEach((u, i) => {
        if (u.isDelisted) return;
        markets.push({
          kind: "perp",
          coin: u.name,
          asset: i,
          szDecimals: u.szDecimals,
          maxLeverage: u.maxLeverage,
          onlyIsolated: !!u.onlyIsolated,
          base: u.name,
          quote: "USD",
        });
        ctxs[u.name] = actxs[i] ?? {};
      });
      const [smeta, sctxs] = spot as unknown as [
        {
          universe: { name: string; tokens: [number, number]; index: number; isCanonical: boolean }[];
          tokens: { name: string; szDecimals: number; index: number }[];
        },
        (MarketCtx & { coin: string })[],
      ];
      const midMap = mids as Record<string, string>;
      for (const p of smeta.universe) {
        // Only pairs the node actually trades (it reports a mid for them).
        if (!(p.name in midMap)) continue;
        const base = smeta.tokens.find((t) => t.index === p.tokens[0]);
        const quote = smeta.tokens.find((t) => t.index === p.tokens[1]);
        markets.push({
          kind: "spot",
          coin: p.name,
          asset: SPOT_OFFSET + p.index,
          szDecimals: base?.szDecimals ?? 0,
          maxLeverage: 1,
          onlyIsolated: false,
          base: base?.name ?? p.name,
          quote: quote?.name ?? "USDC",
        });
        ctxs[p.name] = sctxs.find((x) => x.coin === p.name) ?? {};
      }
      setState((s) => ({ markets, ctxs, mids: { ...s.mids, ...midMap }, error: null, loaded: true }));
    } catch (e) {
      setState((s) => ({ ...s, error: `cannot reach ${c.apiUrl}: ${errMsg(e)}` }));
    }
  }, [c]);

  usePoll(load, 5000, [load]);

  useSub(
    c.subs,
    (subs) =>
      subs.allMids((d) => {
        const mids = (d as { mids: Record<string, string> }).mids;
        setState((s) => ({ ...s, mids: { ...s.mids, ...mids } }));
      }),
    [],
  );

  return state;
}

/** Live context for the selected market (WS activeAssetCtx / activeSpotAssetCtx). */
export function useActiveCtx(c: Clients, m: Market | null): MarketCtx | null {
  const [ctx, setCtx] = useState<MarketCtx | null>(null);
  useEffect(() => setCtx(null), [m?.coin]);
  useSub(
    m ? c.subs : null,
    m
      ? (subs) =>
          m.kind === "perp"
            ? subs.activeAssetCtx({ coin: m.coin }, (d) => setCtx(d.ctx as MarketCtx))
            : subs.activeSpotAssetCtx({ coin: m.coin }, (d) => setCtx(d.ctx as MarketCtx))
      : null,
    [m?.coin, m?.kind],
  );
  return ctx;
}

export interface LiveBook {
  book: Book | null;
  source: "ws" | "http" | null;
}

/** l2Book: HTTP snapshot, then WS; falls back to HTTP polling when WS is silent. */
export function useBook(c: Clients, coin: string | null): LiveBook {
  const [st, setSt] = useState<LiveBook>({ book: null, source: null });
  const lastWs = useRef(0);
  useEffect(() => {
    setSt({ book: null, source: null });
    lastWs.current = 0;
  }, [coin]);

  usePoll(
    coin
      ? async () => {
          if (Date.now() - lastWs.current < 4000) return;
          const b = (await c.info.l2Book({ coin })) as unknown as Book | null;
          if (b && Date.now() - lastWs.current >= 4000) setSt({ book: b, source: "http" });
        }
      : null,
    2000,
    [coin, c],
  );

  useSub(
    coin ? c.subs : null,
    coin
      ? (subs) =>
          subs.l2Book({ coin }, (d) => {
            lastWs.current = Date.now();
            setSt({ book: d as unknown as Book, source: "ws" });
          })
      : null,
    [coin],
  );
  return st;
}

/** Recent trades: HTTP snapshot + WS trades. */
export function useTrades(c: Clients, coin: string | null): Trade[] {
  const [trades, setTrades] = useState<Trade[]>([]);
  useEffect(() => {
    setTrades([]);
    if (!coin) return;
    let alive = true;
    c.info
      .recentTrades({ coin })
      .then((t) => alive && setTrades((prev) => merge(prev, t as unknown as Trade[])))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [c, coin]);
  useSub(
    coin ? c.subs : null,
    coin ? (subs) => subs.trades({ coin }, (d) => setTrades((prev) => merge(prev, d as unknown as Trade[]))) : null,
    [coin],
  );
  return trades;
}

function merge(prev: Trade[], add: Trade[]): Trade[] {
  const seen = new Set(prev.map((t) => `${t.tid}:${t.time}:${t.side}`));
  const fresh = add.filter((t) => !seen.has(`${t.tid}:${t.time}:${t.side}`));
  return [...fresh, ...prev].sort((a, b) => b.time - a.time || b.tid - a.tid).slice(0, 80);
}

export const INTERVALS = ["1m", "5m", "15m", "1h", "4h", "1d"] as const;
export type Interval = (typeof INTERVALS)[number];
const IV_MS: Record<Interval, number> = {
  "1m": 60e3,
  "5m": 300e3,
  "15m": 900e3,
  "1h": 3600e3,
  "4h": 14400e3,
  "1d": 86400e3,
};

/** candleSnapshot (last ~300 bars) + WS candle updates. */
export function useCandles(c: Clients, coin: string | null, iv: Interval): { candles: Candle[]; rev: number } {
  const [st, setSt] = useState<{ candles: Candle[]; rev: number }>({ candles: [], rev: 0 });
  useEffect(() => {
    setSt((s) => ({ candles: [], rev: s.rev + 1 }));
    if (!coin) return;
    let alive = true;
    c.info
      .candleSnapshot({ coin, interval: iv, startTime: Date.now() - 300 * IV_MS[iv] })
      .then((cs) => {
        if (!alive) return;
        setSt((s) => ({ candles: upsert(s.candles, cs as unknown as Candle[]), rev: s.rev + 1 }));
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [c, coin, iv]);
  useSub(
    coin ? c.subs : null,
    coin
      ? (subs) =>
          subs.candle({ coin, interval: iv }, (d) =>
            setSt((s) => ({ candles: upsert(s.candles, [d as unknown as Candle]), rev: s.rev })),
          )
      : null,
    [coin, iv],
  );
  return st;
}

function upsert(prev: Candle[], add: Candle[]): Candle[] {
  const m = new Map(prev.map((x) => [x.t, x]));
  for (const x of add) m.set(x.t, x);
  return [...m.values()].sort((a, b) => a.t - b.t);
}

/** WS connection state, polled from the transport's socket. */
export function useWsState(c: Clients): "open" | "connecting" | "closed" {
  const [s, setS] = useState<"open" | "connecting" | "closed">("connecting");
  useEffect(() => {
    const id = setInterval(() => {
      const rs = c.ws.socket.readyState;
      setS(rs === 1 ? "open" : rs === 0 ? "connecting" : "closed");
    }, 1000);
    return () => clearInterval(id);
  }, [c]);
  return s;
}
