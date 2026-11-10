import { formatPrice, formatSize } from "@nktkas/hyperliquid/utils";
import type { Market } from "./types";

export function num(s: string | number | null | undefined): number {
  if (s === null || s === undefined || s === "") return NaN;
  return typeof s === "number" ? s : Number(s);
}

/** Display a number with fixed decimals and thousands separators. */
export function fmt(s: string | number | null | undefined, dp = 2): string {
  const n = num(s);
  if (!Number.isFinite(n)) return "–";
  return n.toLocaleString("en-US", { minimumFractionDigits: dp, maximumFractionDigits: dp });
}

/** Display a price string as the server sent it (trim trailing ".0"). */
export function px(s: string | null | undefined): string {
  if (s === null || s === undefined) return "–";
  const n = num(s);
  if (!Number.isFinite(n)) return "–";
  const [i, f] = s.split(".");
  const int = Number(i).toLocaleString("en-US");
  return f && f !== "0" ? `${int}.${f}` : int;
}

/** Computed prices (e.g. liquidation px) to 5 significant figures. */
export function sigPx(s: string | null | undefined): string {
  const n = num(s);
  if (!Number.isFinite(n)) return "–";
  return Number(n.toPrecision(5)).toLocaleString("en-US", { maximumFractionDigits: 8 });
}

export function usd(s: string | number | null | undefined): string {
  const n = num(s);
  if (!Number.isFinite(n)) return "–";
  return (n < 0 ? "-$" : "$") + fmt(Math.abs(n), 2);
}

export function pct(s: string | number | null | undefined, dp = 4): string {
  const n = num(s);
  if (!Number.isFinite(n)) return "–";
  return (n * 100).toFixed(dp) + "%";
}

export function time(ms: number): string {
  const d = new Date(ms);
  return d.toLocaleTimeString("en-GB", { hour12: false });
}

export function dateTime(ms: number): string {
  const d = new Date(ms);
  return `${d.toLocaleDateString("en-CA")} ${d.toLocaleTimeString("en-GB", { hour12: false })}`;
}

/** Price rounded to the HL tick rules (5 significant figures, max decimals). */
export function roundPx(price: string | number, m: Market): string {
  return formatPrice(price, m.szDecimals, m.kind);
}

/** Size truncated to the asset's szDecimals. */
export function roundSz(size: string | number, m: Market): string {
  return formatSize(size, m.szDecimals);
}

/** Aggressive limit price for a market (IOC) order, as the SDK examples do. */
export function slippagePx(ref: number, isBuy: boolean, slippage: number, m: Market): string {
  return roundPx(ref * (isBuy ? 1 + slippage : 1 - slippage), m);
}

/** Smallest tick / lot as display hints. */
export function szStep(m: Market): string {
  return m.szDecimals === 0 ? "1" : "0." + "0".repeat(m.szDecimals - 1) + "1";
}

export function errMsg(e: unknown): string {
  if (e instanceof Error) return e.message;
  return String(e);
}
