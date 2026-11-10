import { useEffect, useRef } from "react";
import type { SubscriptionClient } from "@nktkas/hyperliquid";

type Unsub = { unsubscribe: () => Promise<void> };

/**
 * Subscribe while mounted / while deps are unchanged. Handles the
 * subscribe-resolves-after-unmount race and reports failures via onError.
 */
export function useSub(
  subs: SubscriptionClient | null,
  start: ((subs: SubscriptionClient) => Promise<Unsub>) | null,
  deps: unknown[],
  onError?: (e: unknown) => void,
) {
  const errRef = useRef(onError);
  errRef.current = onError;
  useEffect(() => {
    if (!subs || !start) return;
    let cancelled = false;
    let handle: Unsub | null = null;
    start(subs).then(
      (h) => {
        if (cancelled) h.unsubscribe().catch(() => {});
        else handle = h;
      },
      (e) => {
        if (!cancelled) errRef.current?.(e);
      },
    );
    return () => {
      cancelled = true;
      handle?.unsubscribe().catch(() => {});
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [subs, ...deps]);
}

/** Run `fn` now and every `ms` while deps are unchanged. */
export function usePoll(fn: (() => void | Promise<void>) | null, ms: number, deps: unknown[]) {
  const fnRef = useRef(fn);
  fnRef.current = fn;
  useEffect(() => {
    if (!fnRef.current) return;
    let alive = true;
    const tick = () => {
      if (alive) void Promise.resolve(fnRef.current?.()).catch(() => {});
    };
    tick();
    const id = setInterval(tick, ms);
    return () => {
      alive = false;
      clearInterval(id);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ms, fn === null, ...deps]);
}
