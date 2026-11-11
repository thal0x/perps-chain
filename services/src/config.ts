// Configuration: built-in defaults, then an optional JSON file
// (services.config.json next to package.json, or --config <file>), then
// environment variables, then command-line flags. Every knob has a default
// that suits a laptop localnet, so normally nothing needs to be set.
import { existsSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const SERVICES_DIR = resolve(dirname(fileURLToPath(import.meta.url)), "..");

export interface FaucetConfig {
  /** Run the faucet HTTP service in this process. */
  enabled: boolean;
  host: string;
  port: number;
  /** USDC (perp balance) sent per drip. */
  usdcAmount: number;
  /** PURR (spot) sent per drip, if the faucet holds PURR. 0 disables. */
  purrAmount: number;
  /** One drip per address per this many minutes. */
  addressCooldownMin: number;
  /** At most this many drips per client IP per `ipWindowMin` (bots with the local token are exempt). */
  ipMaxDrips: number;
  ipWindowMin: number;
  /** Total USDC the faucet hands out per rolling 24 h. */
  dailyCapUsdc: number;
  /**
   * Faucet key: "auto" = the last unnamed funded account in accounts.json
   * (never used by the seeding script or the bots). Env FAUCET_KEY overrides.
   */
  key: string;
}

export interface MmConfig {
  enabled: boolean;
  /** Perp coins to quote ("all" = every perp in meta). */
  perps: string[] | "all";
  /** Also quote the PURR/USDC spot book. */
  spot: boolean;
  /** Price levels per side. */
  levels: number;
  /** Distance of the best bid/ask from the reference price, in basis points (1 bp = 0.01%). */
  halfSpreadBps: number;
  /** Extra distance between consecutive levels, bps. */
  levelStepBps: number;
  /** USD value of the first level; deeper levels grow by `levelSizeGrowth` each. */
  levelUsd: number;
  levelSizeGrowth: number;
  /** Refresh period, ms. */
  intervalMs: number;
  /** Re-quote a book only when the reference moved more than this (bps), or a level was filled. */
  requoteBps: number;
  /** Hard cap on |position| per perp market, in USD. Never quotes the side that would exceed it. */
  maxPositionUsd: number;
  /** Quotes shift by up to this many bps (at the cap) to lean against the position. */
  maxSkewBps: number;
  /** Cross leverage set on every perp market. */
  leverage: number;
  /** PURR the MM tries to hold for spot asks, and the spot inventory cap (PURR either side of it). */
  spotTargetPurr: number;
  spotMaxDeviationPurr: number;
  /** Spot USDC to keep for spot bids (moved from the perp balance when low). */
  spotUsdc: number;
  /** Key: "auto" = the second-to-last unnamed funded account in accounts.json. Env MM_KEY overrides. */
  key: string;
  /** Send MM equity above `sweepAboveUsd` back to the faucet (0 disables). */
  sweepAboveUsd: number;
}

export interface TraderConfig {
  enabled: boolean;
  /** Number of trader wallets (personalities cycle bull, bear, reverter, degen). */
  count: number;
  /** One knob for pace: 1 = each wallet acts every ~5-20 s; 2 = twice as often; 0.5 = half. */
  activity: number;
  /** Ask the faucet for a top-up when withdrawable USDC falls below this. */
  topUpBelowUsd: number;
  /** Fraction of a wallet's actions that trade PURR/USDC spot. */
  spotShare: number;
  /** Coins traders pick from ("all" = every perp). */
  perps: string[] | "all";
}

export interface Config {
  /** Localnet directory (accounts.json, validators.json). */
  localnetDir: string;
  /** Node API; default: node0's api_addr from validators.json, else http://127.0.0.1:3001. */
  apiUrl: string;
  /** Faucet URL the bots call; default http://<faucet.host>:<faucet.port>. */
  faucetUrl: string;
  /** Where logs and bot wallets go; default <localnetDir>/services and <localnetDir>/bots. */
  logDir: string;
  botsDir: string;
  /** Move PURR from the sdk_example account to the MM / faucet when they hold none (one-off bootstrap). */
  bootstrapPurr: boolean;
  faucet: FaucetConfig;
  mm: MmConfig;
  traders: TraderConfig;
}

const DEFAULTS: Config = {
  localnetDir: resolve(SERVICES_DIR, "../run/localnet"),
  apiUrl: "",
  faucetUrl: "",
  logDir: "",
  botsDir: "",
  bootstrapPurr: true,
  faucet: {
    enabled: true,
    host: "127.0.0.1",
    port: 3100,
    usdcAmount: 10_000,
    purrAmount: 100,
    addressCooldownMin: 10,
    ipMaxDrips: 5,
    ipWindowMin: 10,
    dailyCapUsdc: 500_000,
    key: "auto",
  },
  mm: {
    enabled: true,
    perps: "all",
    spot: true,
    levels: 5,
    halfSpreadBps: 5,
    levelStepBps: 5,
    levelUsd: 3_000,
    levelSizeGrowth: 1.5,
    intervalMs: 1_500,
    requoteBps: 4,
    maxPositionUsd: 50_000,
    maxSkewBps: 25,
    leverage: 3,
    spotTargetPurr: 20_000,
    spotMaxDeviationPurr: 15_000,
    spotUsdc: 50_000,
    key: "auto",
    sweepAboveUsd: 0,
  },
  traders: {
    enabled: true,
    count: 4,
    activity: 1,
    topUpBelowUsd: 2_000,
    spotShare: 0.12,
    perps: "all",
  },
};

type Json = Record<string, unknown>;

function merge<T>(base: T, over: unknown): T {
  if (!over || typeof over !== "object" || Array.isArray(over)) return base;
  const out: Json = { ...(base as Json) };
  for (const [k, v] of Object.entries(over as Json)) {
    if (!(k in out)) throw new Error(`unknown config key "${k}"`);
    const cur = out[k];
    out[k] = cur && typeof cur === "object" && !Array.isArray(cur) && v && typeof v === "object" && !Array.isArray(v) ? merge(cur, v) : v;
  }
  return out as T;
}

/** Sets a dotted path ("mm.levels") from a string, coercing to the default's type. */
function setPath(cfg: Config, path: string, raw: string) {
  const parts = path.split(".");
  let o: Json = cfg as unknown as Json;
  for (const p of parts.slice(0, -1)) {
    if (!o[p] || typeof o[p] !== "object") throw new Error(`unknown config key "${path}"`);
    o = o[p] as Json;
  }
  const k = parts[parts.length - 1];
  if (!(k in o)) throw new Error(`unknown config key "${path}"`);
  const cur = o[k];
  if (typeof cur === "number") {
    const n = Number(raw);
    if (!Number.isFinite(n)) throw new Error(`${path}: not a number: ${raw}`);
    o[k] = n;
  } else if (typeof cur === "boolean") {
    o[k] = /^(1|true|yes|on)$/i.test(raw);
  } else if (Array.isArray(cur) || k === "perps") {
    o[k] = raw === "all" ? "all" : raw.split(",").map((s) => s.trim()).filter(Boolean);
  } else {
    o[k] = raw;
  }
}

// Short env names for the common knobs.
const ENV: Record<string, string> = {
  LOCALNET_DIR: "localnetDir",
  HL_API: "apiUrl",
  FAUCET_URL: "faucetUrl",
  FAUCET_PORT: "faucet.port",
  FAUCET_HOST: "faucet.host",
  FAUCET_AMOUNT: "faucet.usdcAmount",
  FAUCET_COOLDOWN_MIN: "faucet.addressCooldownMin",
  FAUCET_DAILY_CAP: "faucet.dailyCapUsdc",
  ACTIVITY: "traders.activity",
  TRADERS: "traders.count",
  TOPUP_BELOW: "traders.topUpBelowUsd",
  MM_SPREAD_BPS: "mm.halfSpreadBps",
  MM_LEVELS: "mm.levels",
  MM_MAX_POSITION: "mm.maxPositionUsd",
};

export interface Cli {
  config: Config;
  only: "all" | "faucet" | "bots" | "mm" | "traders";
}

export function loadConfig(argv: string[] = process.argv.slice(2), env = process.env): Cli {
  let cfg: Config = structuredClone(DEFAULTS);
  let only: Cli["only"] = "all";
  const sets: [string, string][] = [];
  let file = resolve(SERVICES_DIR, "services.config.json");
  let fileExplicit = false;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const val = () => {
      const v = argv[++i];
      if (v === undefined) throw new Error(`${a} needs a value`);
      return v;
    };
    if (a === "--config") {
      file = resolve(val());
      fileExplicit = true;
    } else if (a === "--only") {
      const v = val();
      if (!["all", "faucet", "bots", "mm", "traders"].includes(v)) throw new Error(`--only: faucet|bots|mm|traders`);
      only = v as Cli["only"];
    } else if (a === "--localnet") sets.push(["localnetDir", resolve(val())]);
    else if (a === "--api") sets.push(["apiUrl", val()]);
    else if (a === "--set") {
      const v = val();
      const eq = v.indexOf("=");
      if (eq < 0) throw new Error(`--set key=value`);
      sets.push([v.slice(0, eq), v.slice(eq + 1)]);
    } else if (a === "-h" || a === "--help") {
      console.log(
        "usage: npm start -- [--localnet DIR] [--api URL] [--only faucet|bots|mm|traders] [--config FILE] [--set key=value ...]\n" +
          "see services/README.md for the knobs",
      );
      process.exit(0);
    } else throw new Error(`unknown argument ${a}`);
  }
  if (existsSync(file)) cfg = merge(cfg, JSON.parse(readFileSync(file, "utf8")));
  else if (fileExplicit) throw new Error(`config file not found: ${file}`);
  for (const [name, path] of Object.entries(ENV)) {
    const v = env[name];
    if (v !== undefined && v !== "") setPath(cfg, path, v);
  }
  for (const [k, v] of sets) setPath(cfg, k, v);

  cfg.localnetDir = resolve(cfg.localnetDir);
  if (!cfg.apiUrl) cfg.apiUrl = apiFromLocalnet(cfg.localnetDir) ?? "http://127.0.0.1:3001";
  cfg.apiUrl = cfg.apiUrl.replace(/\/+$/, "");
  if (!cfg.faucetUrl) cfg.faucetUrl = `http://${cfg.faucet.host === "0.0.0.0" ? "127.0.0.1" : cfg.faucet.host}:${cfg.faucet.port}`;
  if (!cfg.logDir) cfg.logDir = resolve(cfg.localnetDir, "services");
  if (!cfg.botsDir) cfg.botsDir = resolve(cfg.localnetDir, "bots");
  if (only === "faucet") {
    cfg.mm.enabled = false;
    cfg.traders.enabled = false;
  } else if (only === "bots") cfg.faucet.enabled = false;
  else if (only === "mm") {
    cfg.faucet.enabled = false;
    cfg.traders.enabled = false;
  } else if (only === "traders") {
    cfg.faucet.enabled = false;
    cfg.mm.enabled = false;
  }
  if (cfg.mm.levels < 1 || cfg.mm.levels > 20) throw new Error("mm.levels must be 1..20");
  if (cfg.traders.activity <= 0) throw new Error("traders.activity must be > 0");
  return { config: cfg, only };
}

function apiFromLocalnet(dir: string): string | null {
  try {
    const v = JSON.parse(readFileSync(resolve(dir, "validators.json"), "utf8")) as { api_addr?: string }[];
    return v[0]?.api_addr ? `http://${v[0].api_addr}` : null;
  } catch {
    return null;
  }
}
