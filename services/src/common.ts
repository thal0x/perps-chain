// Shared helpers: logging, localnet accounts, SDK clients, market metadata,
// price/size rounding and small utilities.
import { ApiRequestError, ExchangeClient, HttpTransport, InfoClient } from "@nktkas/hyperliquid";
import { formatPrice, formatSize } from "@nktkas/hyperliquid/utils";
import { appendFileSync, chmodSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { generatePrivateKey, privateKeyToAccount, type PrivateKeyAccount } from "viem/accounts";

// ------------------------------------------------------------------ logging

export interface Logger {
  info(msg: string): void;
  warn(msg: string): void;
  error(msg: string): void;
}

let logDir = "";
export function setLogDir(dir: string) {
  mkdirSync(dir, { recursive: true });
  logDir = dir;
}

function stamp() {
  return new Date().toISOString().replace("T", " ").replace("Z", "");
}

/** One line per event to the console (`[name] msg`) and to <logDir>/<file>.log with a timestamp. */
export function logger(name: string, file = name): Logger {
  const write = (lvl: string, msg: string) => {
    const tag = lvl === "info" ? "" : `${lvl.toUpperCase()} `;
    const line = `[${name}] ${tag}${msg}`;
    if (lvl === "error") console.error(`${stamp().slice(11, 19)} ${line}`);
    else console.log(`${stamp().slice(11, 19)} ${line}`);
    if (logDir) {
      try {
        appendFileSync(resolve(logDir, `${file}.log`), `${stamp()} ${line}\n`);
      } catch {
        /* log dir vanished (localnet wiped): console only */
      }
    }
  };
  return { info: (m) => write("info", m), warn: (m) => write("warn", m), error: (m) => write("error", m) };
}

// ------------------------------------------------------------------ accounts

export interface LocalAccount {
  address: string;
  secret: string;
  name?: string | null;
}

export function readAccounts(localnetDir: string): LocalAccount[] {
  const f = resolve(localnetDir, "accounts.json");
  if (!existsSync(f)) throw new Error(`${f} not found: start a localnet first (scripts/localnet.sh dev)`);
  return JSON.parse(readFileSync(f, "utf8")) as LocalAccount[];
}

/** Unnamed funded test accounts, in file order. #0 and #1 are used by `localnet.sh seed`. */
export function testAccounts(accts: LocalAccount[]): LocalAccount[] {
  return accts.filter((a) => !a.name);
}

export function normalizeKey(k: string): `0x${string}` {
  const s = k.trim().replace(/^0x/i, "");
  if (!/^[0-9a-fA-F]{64}$/.test(s)) throw new Error("not a 32-byte hex private key");
  return `0x${s.toLowerCase()}`;
}

export function isAddress(a: unknown): a is string {
  return typeof a === "string" && /^0x[0-9a-fA-F]{40}$/.test(a);
}

/** Bot wallets: random keys created once and kept in <botsDir>/wallets.json (mode 600). */
export function botWallets(botsDir: string, names: string[]): Record<string, `0x${string}`> {
  mkdirSync(botsDir, { recursive: true });
  const f = resolve(botsDir, "wallets.json");
  let w: Record<string, `0x${string}`> = {};
  if (existsSync(f)) w = JSON.parse(readFileSync(f, "utf8"));
  let changed = false;
  for (const n of names) {
    if (!w[n]) {
      w[n] = generatePrivateKey();
      changed = true;
    }
  }
  if (changed) {
    writeFileSync(f, JSON.stringify(w, null, 2) + "\n", { mode: 0o600 });
    chmodSync(f, 0o600);
  }
  return w;
}

// ------------------------------------------------------------------ clients

export interface Wallet {
  account: PrivateKeyAccount;
  address: `0x${string}`;
  exchange: ExchangeClient;
}

export function makeInfo(apiUrl: string): { http: HttpTransport; info: InfoClient } {
  // The local chain signs as testnet (D9).
  const http = new HttpTransport({ apiUrl, isTestnet: true, timeout: 15_000 });
  return { http, info: new InfoClient({ transport: http }) };
}

export function makeWallet(http: HttpTransport, key: string): Wallet {
  const account = privateKeyToAccount(normalizeKey(key));
  return { account, address: account.address, exchange: new ExchangeClient({ transport: http, wallet: account }) };
}

/** The server's error text (ApiRequestError carries it), else the message. */
export function errText(e: unknown): string {
  if (e instanceof ApiRequestError) return e.message;
  if (e instanceof Error) return e.cause instanceof Error ? `${e.message} (${e.cause.message})` : e.message;
  return String(e);
}

/** Per-order statuses of an order/batchModify call, also when the SDK threw because one of them failed. */
export function statusesOf(r: unknown): unknown[] {
  const src = r instanceof ApiRequestError ? r.response : r;
  const st = (src as { response?: { data?: { statuses?: unknown[] } } })?.response?.data?.statuses;
  return Array.isArray(st) ? st : [];
}

// ------------------------------------------------------------------ markets

export interface Market {
  /** Book name: "BTC" or "PURR/USDC". */
  coin: string;
  /** Asset id for actions: perp index, or 10000 + spot pair index. */
  asset: number;
  kind: "perp" | "spot";
  szDecimals: number;
  maxLeverage: number;
}

export async function loadMarkets(info: InfoClient): Promise<Market[]> {
  const [meta, spot] = await Promise.all([info.meta(), info.spotMeta()]);
  const out: Market[] = meta.universe.map((u, i) => ({
    coin: u.name,
    asset: i,
    kind: "perp" as const,
    szDecimals: u.szDecimals,
    maxLeverage: u.maxLeverage,
  }));
  for (const p of spot.universe) {
    if (!p.isCanonical) continue;
    const base = spot.tokens.find((t) => t.index === p.tokens[0]);
    out.push({ coin: p.name, asset: 10_000 + p.index, kind: "spot", szDecimals: base?.szDecimals ?? 0, maxLeverage: 1 });
  }
  return out;
}

/** Price string valid for the market (5 significant figures, max decimals); rounded down. */
export function fmtPx(px: number, m: Market): string {
  return formatPrice(px.toFixed(12), m.szDecimals, m.kind);
}

/** Size truncated to szDecimals; "" if it rounds to zero. */
export function fmtSz(sz: number, m: Market): string {
  if (!(sz > 0)) return "";
  const s = formatSize(sz.toFixed(12), m.szDecimals);
  return Number(s) > 0 ? s : "";
}

/** Smallest valid size. */
export function lot(m: Market): number {
  return 10 ** -m.szDecimals;
}

// ------------------------------------------------------------------ misc

export const sleep = (ms: number, signal?: AbortSignal) =>
  new Promise<void>((res) => {
    if (signal?.aborted) return res();
    const t = setTimeout(res, ms);
    signal?.addEventListener(
      "abort",
      () => {
        clearTimeout(t);
        res();
      },
      { once: true },
    );
  });

export const rand = (a: number, b: number) => a + Math.random() * (b - a);
export const chance = (p: number) => Math.random() < p;
export function pick<T>(xs: readonly T[]): T {
  return xs[Math.floor(Math.random() * xs.length)];
}
export const num = (s: string | number | null | undefined) => (s === null || s === undefined || s === "" ? NaN : Number(s));
export const usd = (n: number) => `$${n.toLocaleString("en-US", { maximumFractionDigits: 2, minimumFractionDigits: 2 })}`;
export const short = (a: string) => `${a.slice(0, 6)}…${a.slice(-4)}`;

/** PURR token string for spotSend: "PURR:0x<tokenId>". */
export async function spotTokenString(info: InfoClient, name: string): Promise<string | null> {
  const sm = await info.spotMeta();
  const t = sm.tokens.find((x) => x.name === name);
  return t ? `${t.name}:${t.tokenId}` : null;
}

export async function spotBalance(info: InfoClient, user: string, coin: string): Promise<{ total: number; hold: number }> {
  const s = await info.spotClearinghouseState({ user });
  const b = s.balances.find((x) => x.coin === coin);
  return { total: num(b?.total ?? "0"), hold: num(b?.hold ?? "0") };
}
