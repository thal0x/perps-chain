// Faucet: hands out test USDC (and a little PURR) from a funded localnet account.
//   POST /drip {"address": "0x..."}  -> sends usdSend (+ spotSend PURR if the faucet holds any)
//   GET  /status                      -> faucet balance, limits, recent drips
// Limits: one drip per address per cooldown, N drips per client IP per window
// (callers presenting the local bot token are exempt from the IP limit only),
// and a rolling 24 h USDC cap. State survives restarts in <logDir>/faucet-state.json.
import { randomBytes, timingSafeEqual } from "node:crypto";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { resolve } from "node:path";
import type { Config } from "./config.ts";
import {
  errText,
  isAddress,
  logger,
  makeInfo,
  makeWallet,
  num,
  readAccounts,
  short,
  spotBalance,
  spotTokenString,
  testAccounts,
  type Wallet,
} from "./common.ts";

const log = logger("faucet");

interface Drip {
  t: number;
  address: string;
  ip: string;
  usdc: number;
  purr: number;
  bot: boolean;
}

export const TOKEN_FILE = "faucet.token";

/** Shared secret that lets local bots skip the per-IP limit. Created once, mode 600. */
export function faucetToken(logDir: string, create: boolean): string | null {
  const f = resolve(logDir, TOKEN_FILE);
  if (existsSync(f)) return readFileSync(f, "utf8").trim();
  if (!create) return null;
  const t = randomBytes(24).toString("hex");
  writeFileSync(f, t + "\n", { mode: 0o600 });
  return t;
}

export function faucetKey(cfg: Config): { key: string; source: string } {
  if (process.env.FAUCET_KEY) return { key: process.env.FAUCET_KEY, source: "env FAUCET_KEY" };
  if (cfg.faucet.key && cfg.faucet.key !== "auto") return { key: cfg.faucet.key, source: "config faucet.key" };
  const t = testAccounts(readAccounts(cfg.localnetDir));
  if (t.length < 4) throw new Error("accounts.json has fewer than 4 funded test accounts; set FAUCET_KEY");
  return { key: t[t.length - 1].secret, source: `accounts.json test account #${t.length - 1}` };
}

export interface FaucetHandle {
  address: string;
  close(): Promise<void>;
}

export async function startFaucet(cfg: Config): Promise<FaucetHandle> {
  const fc = cfg.faucet;
  const { http, info } = makeInfo(cfg.apiUrl);
  const { key, source } = faucetKey(cfg);
  const wallet: Wallet = makeWallet(http, key);
  const token = faucetToken(cfg.logDir, true)!;
  const purrToken = fc.purrAmount > 0 ? await spotTokenString(info, "PURR").catch(() => null) : null;
  const stateFile = resolve(cfg.logDir, "faucet-state.json");
  let drips: Drip[] = [];
  try {
    if (existsSync(stateFile)) drips = JSON.parse(readFileSync(stateFile, "utf8")).drips ?? [];
  } catch {
    drips = [];
  }
  const pending = new Set<string>();
  const DAY = 24 * 3600_000;

  const save = () => {
    drips = drips.filter((d) => d.t > Date.now() - DAY);
    try {
      writeFileSync(stateFile, JSON.stringify({ drips }, null, 1));
    } catch (e) {
      log.warn(`could not save state: ${errText(e)}`);
    }
  };

  const balances = async () => {
    const [ch, purr, usdcSpot] = await Promise.all([
      info.clearinghouseState({ user: wallet.address }),
      spotBalance(info, wallet.address, "PURR"),
      spotBalance(info, wallet.address, "USDC"),
    ]);
    return {
      usdcWithdrawable: num(ch.withdrawable),
      usdcAccountValue: num(ch.marginSummary.accountValue),
      spotUsdc: usdcSpot.total - usdcSpot.hold,
      purr: purr.total - purr.hold,
    };
  };

  const isBot = (req: IncomingMessage) => {
    const h = req.headers["x-faucet-token"];
    if (typeof h !== "string" || h.length !== token.length) return false;
    return timingSafeEqual(Buffer.from(h), Buffer.from(token));
  };

  /** Rate-limit check; returns an error + seconds to wait, or null if allowed. */
  const limited = (address: string, ip: string, bot: boolean, now: number): { error: string; retryAfterSec: number } | null => {
    const a = address.toLowerCase();
    if (pending.has(a)) return { error: "a drip to this address is already in progress", retryAfterSec: 5 };
    const cool = fc.addressCooldownMin * 60_000;
    const lastA = drips.filter((d) => d.address === a).reduce((m, d) => Math.max(m, d.t), 0);
    if (lastA && now - lastA < cool) {
      const s = Math.ceil((lastA + cool - now) / 1000);
      return { error: `rate limited: this address got funds ${Math.round((now - lastA) / 1000)} s ago; try again in ${fmtWait(s)}`, retryAfterSec: s };
    }
    if (!bot) {
      const win = fc.ipWindowMin * 60_000;
      const fromIp = drips.filter((d) => d.ip === ip && !d.bot && now - d.t < win).sort((x, y) => x.t - y.t);
      if (fromIp.length >= fc.ipMaxDrips) {
        const s = Math.ceil((fromIp[fromIp.length - fc.ipMaxDrips].t + win - now) / 1000);
        return { error: `rate limited: ${fc.ipMaxDrips} drips from your IP in the last ${fc.ipWindowMin} min; try again in ${fmtWait(s)}`, retryAfterSec: s };
      }
    }
    const day = drips.filter((d) => now - d.t < DAY);
    const used = day.reduce((s, d) => s + d.usdc, 0);
    if (used + fc.usdcAmount > fc.dailyCapUsdc) {
      const oldest = day.sort((x, y) => x.t - y.t)[0];
      const s = oldest ? Math.ceil((oldest.t + DAY - now) / 1000) : 3600;
      return { error: `faucet daily cap of ${fc.dailyCapUsdc} USDC reached; try again in ${fmtWait(s)}`, retryAfterSec: s };
    }
    return null;
  };

  const drip = async (address: string, ip: string, bot: boolean) => {
    const now = Date.now();
    if (address.toLowerCase() === wallet.address.toLowerCase()) return { code: 400, body: { error: "that is the faucet's own address" } };
    const lim = limited(address, ip, bot, now);
    if (lim) {
      log.info(`drip ${short(address)} ip=${ip}${bot ? " (bot)" : ""} REJECTED: ${lim.error}`);
      return { code: 429, body: { ...lim } };
    }
    const a = address.toLowerCase();
    pending.add(a);
    try {
      const bal = await balances();
      if (bal.usdcWithdrawable < fc.usdcAmount) {
        log.warn(`drip ${short(address)} FAILED: faucet has only ${bal.usdcWithdrawable} USDC withdrawable`);
        return { code: 503, body: { error: `faucet is empty (${bal.usdcWithdrawable.toFixed(2)} USDC left)` } };
      }
      await wallet.exchange.usdSend({ destination: address as `0x${string}`, amount: String(fc.usdcAmount) });
      let purr = 0;
      let purrNote: string | undefined;
      if (purrToken && fc.purrAmount > 0) {
        if (bal.purr >= fc.purrAmount) {
          try {
            await wallet.exchange.spotSend({ destination: address as `0x${string}`, token: purrToken, amount: String(fc.purrAmount) });
            purr = fc.purrAmount;
          } catch (e) {
            purrNote = `PURR not sent: ${errText(e)}`;
          }
        } else purrNote = "faucet has no PURR left";
      }
      drips.push({ t: now, address: a, ip, usdc: fc.usdcAmount, purr, bot });
      save();
      log.info(`drip ${address} ip=${ip}${bot ? " (bot)" : ""}: ${fc.usdcAmount} USDC${purr ? ` + ${purr} PURR` : ""}${purrNote ? ` (${purrNote})` : ""}`);
      return {
        code: 200,
        body: { ok: true, address, usdc: fc.usdcAmount, purr, note: purrNote, nextDripInSec: fc.addressCooldownMin * 60 },
      };
    } catch (e) {
      log.error(`drip ${address} FAILED: ${errText(e)}`);
      return { code: 502, body: { error: `send failed: ${errText(e)}` } };
    } finally {
      pending.delete(a);
    }
  };

  const status = async () => {
    const now = Date.now();
    const day = drips.filter((d) => now - d.t < DAY);
    let bal: Awaited<ReturnType<typeof balances>> | { error: string };
    try {
      bal = await balances();
    } catch (e) {
      bal = { error: errText(e) };
    }
    return {
      address: wallet.address,
      api: cfg.apiUrl,
      balance: bal,
      perDrip: { usdc: fc.usdcAmount, purr: purrToken ? fc.purrAmount : 0 },
      limits: {
        addressCooldownMin: fc.addressCooldownMin,
        ipMaxDrips: fc.ipMaxDrips,
        ipWindowMin: fc.ipWindowMin,
        dailyCapUsdc: fc.dailyCapUsdc,
      },
      last24h: { drips: day.length, usdc: day.reduce((s, d) => s + d.usdc, 0), purr: day.reduce((s, d) => s + d.purr, 0) },
      recent: day
        .slice(-20)
        .reverse()
        .map((d) => ({ time: new Date(d.t).toISOString(), address: d.address, usdc: d.usdc, purr: d.purr, bot: d.bot })),
    };
  };

  const server = createServer(async (req, res) => {
    cors(res);
    try {
      const url = new URL(req.url ?? "/", "http://x");
      if (req.method === "OPTIONS") return send(res, 204, null);
      if (req.method === "GET" && (url.pathname === "/status" || url.pathname === "/")) return send(res, 200, await status());
      if (req.method === "POST" && url.pathname === "/drip") {
        let body: Record<string, unknown> = {};
        const raw = await readBody(req, 4096);
        if (raw.trim()) {
          try {
            body = JSON.parse(raw);
          } catch {
            return send(res, 400, { error: 'body must be JSON: {"address": "0x..."}' });
          }
        }
        const address = (body?.address ?? url.searchParams.get("address")) as unknown;
        if (!isAddress(address)) return send(res, 400, { error: "invalid address: expected 0x followed by 40 hex characters" });
        const r = await drip(address, clientIp(req), isBot(req));
        if (r.code === 429) res.setHeader("retry-after", String((r.body as { retryAfterSec: number }).retryAfterSec));
        return send(res, r.code, r.body);
      }
      return send(res, 404, { error: "not found: use POST /drip or GET /status" });
    } catch (e) {
      log.error(`request failed: ${errText(e)}`);
      return send(res, 500, { error: errText(e) });
    }
  });

  await new Promise<void>((ok, fail) => {
    server.once("error", fail);
    server.listen(fc.port, fc.host, () => ok());
  });
  const b = await balances().catch(() => null);
  log.info(
    `listening on http://${fc.host}:${fc.port} · faucet ${wallet.address} (${source}) · ` +
      (b ? `${b.usdcWithdrawable.toFixed(0)} USDC, ${b.purr} PURR · ` : "") +
      `drip ${fc.usdcAmount} USDC${purrToken && fc.purrAmount ? ` + ${fc.purrAmount} PURR` : ""}, 1 per address per ${fc.addressCooldownMin} min`,
  );
  return {
    address: wallet.address,
    close: () => new Promise<void>((ok) => server.close(() => ok())),
  };
}

function fmtWait(s: number) {
  return s >= 120 ? `${Math.ceil(s / 60)} min` : `${s} s`;
}

function cors(res: ServerResponse) {
  res.setHeader("access-control-allow-origin", "*");
  res.setHeader("access-control-allow-methods", "GET, POST, OPTIONS");
  res.setHeader("access-control-allow-headers", "content-type, x-faucet-token");
  res.setHeader("access-control-max-age", "600");
}

function send(res: ServerResponse, code: number, body: unknown) {
  if (body === null) {
    res.writeHead(code);
    res.end();
    return;
  }
  res.writeHead(code, { "content-type": "application/json" });
  res.end(JSON.stringify(body, null, 1));
}

function clientIp(req: IncomingMessage): string {
  return (req.socket.remoteAddress ?? "?").replace(/^::ffff:/, "");
}

function readBody(req: IncomingMessage, max: number): Promise<string> {
  return new Promise((ok, fail) => {
    let n = 0;
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => {
      n += c.length;
      if (n > max) {
        fail(new Error("body too large"));
        req.destroy();
      } else chunks.push(c);
    });
    req.on("end", () => ok(Buffer.concat(chunks).toString("utf8")));
    req.on("error", fail);
  });
}

/** Client used by the bots. */
export async function requestDrip(
  faucetUrl: string,
  address: string,
  token: string | null,
): Promise<{ ok: true; usdc: number; purr: number } | { ok: false; error: string; retryAfterSec?: number }> {
  try {
    const r = await fetch(`${faucetUrl}/drip`, {
      method: "POST",
      headers: { "content-type": "application/json", ...(token ? { "x-faucet-token": token } : {}) },
      body: JSON.stringify({ address }),
      signal: AbortSignal.timeout(30_000),
    });
    const j = (await r.json()) as { ok?: boolean; usdc?: number; purr?: number; error?: string; retryAfterSec?: number };
    if (r.ok && j.ok) return { ok: true, usdc: j.usdc ?? 0, purr: j.purr ?? 0 };
    return { ok: false, error: j.error ?? `HTTP ${r.status}`, retryAfterSec: j.retryAfterSec };
  } catch (e) {
    return { ok: false, error: `faucet unreachable at ${faucetUrl}: ${errText(e)}` };
  }
}
