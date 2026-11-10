import { ApiRequestError } from "@nktkas/hyperliquid";
import { useCallback, useState } from "react";
import { useToasts } from "./toasts";
import { errMsg } from "./format";

/** Server error text for a failed /exchange call (the SDK raises ApiRequestError with it). */
export function serverError(e: unknown): string {
  if (e instanceof ApiRequestError) return e.message;
  const m = errMsg(e);
  return m || "unknown error";
}

/** Human summary of an order response's statuses. */
export function describeStatuses(statuses: unknown[]): string {
  return statuses
    .map((s) => {
      if (typeof s === "string") return s;
      const o = s as Record<string, Record<string, unknown> | string>;
      if (o.resting && typeof o.resting === "object") return `resting (oid ${o.resting.oid})`;
      if (o.filled && typeof o.filled === "object") return `filled ${o.filled.totalSz} @ ${o.filled.avgPx} (oid ${o.filled.oid})`;
      if (o.error) return `error: ${o.error}`;
      return JSON.stringify(s);
    })
    .join("; ");
}

/** Runs an exchange action, shows a toast with the result or the server's error string. */
export function useAction() {
  const { push } = useToasts();
  const [busy, setBusy] = useState(false);
  const run = useCallback(
    async <T,>(title: string, fn: () => Promise<T>, ok?: (r: T) => string | undefined): Promise<T | undefined> => {
      setBusy(true);
      try {
        const r = await fn();
        push({ kind: "ok", title, body: ok?.(r) });
        return r;
      } catch (e) {
        push({ kind: "err", title: `${title} rejected`, body: serverError(e) });
        return undefined;
      } finally {
        setBusy(false);
      }
    },
    [push],
  );
  return { run, busy };
}
