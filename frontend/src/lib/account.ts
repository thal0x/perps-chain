import { useCallback, useEffect, useState } from "react";
import type { Clients } from "./hl";
import type { ClearinghouseState, Fill, HistoricalOrder, LeverageSetting, OpenOrder, SpotBalance } from "./types";
import { usePoll, useSub } from "./useSub";

export interface AccountData {
  ch: ClearinghouseState | null;
  openOrders: OpenOrder[];
  fills: Fill[];
  history: HistoricalOrder[];
  spot: SpotBalance[];
  loaded: boolean;
}

const EMPTY: AccountData = { ch: null, openOrders: [], fills: [], history: [], spot: [], loaded: false };

/**
 * Account state. Live via WS clearinghouseState / openOrders / userFills /
 * orderUpdates; spot balances and order history (no WS channel on the node
 * for spotState) are refreshed over HTTP on every user event and every 4 s.
 */
export function useAccount(c: Clients, user: string | null): AccountData & { refresh: () => Promise<void> } {
  const [d, setD] = useState<AccountData>(EMPTY);
  useEffect(() => setD(EMPTY), [user, c]);

  const refresh = useCallback(async () => {
    if (!user) return;
    const u = user as `0x${string}`;
    const [ch, oo, fills, hist, spot] = await Promise.all([
      c.info.clearinghouseState({ user: u }),
      c.info.frontendOpenOrders({ user: u }),
      c.info.userFills({ user: u }),
      c.info.historicalOrders({ user: u }),
      c.info.spotClearinghouseState({ user: u }),
    ]);
    setD({
      ch: ch as unknown as ClearinghouseState,
      openOrders: oo as unknown as OpenOrder[],
      fills: (fills as unknown as Fill[]).slice(0, 200),
      history: (hist as unknown as HistoricalOrder[]).slice(0, 200),
      spot: (spot as unknown as { balances: SpotBalance[] }).balances,
      loaded: true,
    });
  }, [c, user]);

  usePoll(user ? refresh : null, 4000, [refresh]);

  useSub(
    user ? c.subs : null,
    user
      ? (subs) =>
          subs.clearinghouseState({ user: user as `0x${string}` }, (e) =>
            setD((s) => ({ ...s, ch: (e as unknown as { clearinghouseState: ClearinghouseState }).clearinghouseState })),
          )
      : null,
    [user],
  );
  useSub(
    user ? c.subs : null,
    user
      ? (subs) =>
          subs.openOrders({ user: user as `0x${string}` }, (e) =>
            setD((s) => ({ ...s, openOrders: (e as unknown as { orders: OpenOrder[] }).orders })),
          )
      : null,
    [user],
  );
  useSub(
    user ? c.subs : null,
    user
      ? (subs) =>
          subs.userFills({ user: user as `0x${string}` }, (e) => {
            const ev = e as unknown as { isSnapshot?: boolean; fills: Fill[] };
            if (ev.isSnapshot) return; // HTTP has the snapshot
            setD((s) => ({ ...s, fills: [...ev.fills, ...s.fills].slice(0, 200) }));
            void refresh().catch(() => {});
          })
      : null,
    [user, refresh],
  );
  useSub(
    user ? c.subs : null,
    user ? (subs) => subs.orderUpdates({ user: user as `0x${string}` }, () => void refresh().catch(() => {})) : null,
    [user, refresh],
  );

  return { ...d, refresh };
}

/** Current leverage setting for (user, perp coin) via WS activeAssetData. */
export function useLeverage(c: Clients, user: string | null, coin: string | null, isPerp: boolean) {
  const [lev, setLev] = useState<LeverageSetting | null>(null);
  const load = useCallback(async () => {
    if (!user || !coin || !isPerp) return;
    const r = (await c.info.activeAssetData({ user: user as `0x${string}`, coin })) as unknown as {
      leverage: LeverageSetting;
    };
    setLev({ type: r.leverage.type, value: r.leverage.value });
  }, [c, user, coin, isPerp]);
  useEffect(() => {
    setLev(null);
    void load().catch(() => {});
  }, [load]);
  useSub(
    user && coin && isPerp ? c.subs : null,
    user && coin && isPerp
      ? (subs) =>
          subs.activeAssetData({ user: user as `0x${string}`, coin }, (d) => {
            const l = (d as unknown as { leverage: LeverageSetting }).leverage;
            setLev({ type: l.type, value: l.value });
          })
      : null,
    [user, coin, isPerp],
  );
  return { lev, reload: load };
}
