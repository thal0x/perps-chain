// Starts the faucet, the market maker and the trader bots against a running
// localnet. `npm start` (all), `npm run faucet`, `npm run bots`.
import { writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { loadConfig, type Config } from "./config.ts";
import {
  botWallets,
  errText,
  loadMarkets,
  logger,
  makeInfo,
  makeWallet,
  readAccounts,
  setLogDir,
  spotBalance,
  spotTokenString,
  testAccounts,
  type Market,
} from "./common.ts";
import { faucetKey, faucetToken, startFaucet, type FaucetHandle } from "./faucet.ts";
import { MarketMaker } from "./mm.ts";
import { PERSONALITIES, Trader } from "./trader.ts";

const log = logger("main", "services");

function mmKey(cfg: Config): { key: string; source: string } {
  if (process.env.MM_KEY) return { key: process.env.MM_KEY, source: "env MM_KEY" };
  if (cfg.mm.key && cfg.mm.key !== "auto") return { key: cfg.mm.key, source: "config mm.key" };
  const t = testAccounts(readAccounts(cfg.localnetDir));
  if (t.length < 4) throw new Error("accounts.json has fewer than 4 funded test accounts; set MM_KEY");
  return { key: t[t.length - 2].secret, source: `accounts.json test account #${t.length - 2}` };
}

/**
 * Only the sdk_example account holds PURR at genesis. Move some to the MM (for
 * asks on PURR/USDC) and to the faucet (for PURR drips) if they have none,
 * leaving sdk_example at least 20,000.
 */
async function bootstrapPurr(cfg: Config, mmAddr: string | null, faucetAddr: string | null) {
  const sdk = readAccounts(cfg.localnetDir).find((a) => a.name === "sdk_example");
  if (!sdk) return;
  const { http, info } = makeInfo(cfg.apiUrl);
  const token = await spotTokenString(info, "PURR");
  if (!token) return;
  const w = makeWallet(http, sdk.secret);
  const plan: [string, string, number, number][] = [];
  if (mmAddr && cfg.mm.spot) plan.push(["market maker", mmAddr, cfg.mm.spotTargetPurr / 4, cfg.mm.spotTargetPurr]);
  if (faucetAddr && cfg.faucet.purrAmount > 0) plan.push(["faucet", faucetAddr, cfg.faucet.purrAmount * 5, cfg.faucet.purrAmount * 100]);
  for (const [who, addr, below, target] of plan) {
    const have = (await spotBalance(info, addr, "PURR")).total;
    if (have >= below) continue;
    const src = await spotBalance(info, w.address, "PURR");
    const amt = Math.floor(Math.min(target - have, src.total - src.hold - 20_000));
    if (amt <= 0) {
      log.warn(`${who} has ${have} PURR and sdk_example has none to spare; PURR features limited`);
      continue;
    }
    await w.exchange.spotSend({ destination: addr as `0x${string}`, token, amount: String(amt) });
    log.info(`bootstrap: sent ${amt} PURR from sdk_example to the ${who}`);
  }
}

async function main() {
  const { config: cfg } = loadConfig();
  setLogDir(cfg.logDir);
  writeFileSync(resolve(cfg.logDir, "services.pid"), String(process.pid) + "\n");
  log.info(`api ${cfg.apiUrl} · localnet ${cfg.localnetDir} · logs ${cfg.logDir}`);

  const { http, info } = makeInfo(cfg.apiUrl);
  let markets: Market[] = [];
  for (let i = 0; ; i++) {
    try {
      markets = await loadMarkets(info);
      break;
    } catch (e) {
      if (i >= 30) throw new Error(`node API at ${cfg.apiUrl} not reachable: ${errText(e)}`);
      if (i === 0) log.info(`waiting for the node API at ${cfg.apiUrl} …`);
      await new Promise((r) => setTimeout(r, 2000));
    }
  }

  const ac = new AbortController();
  const loops: Promise<void>[] = [];
  let faucet: FaucetHandle | null = null;
  let mm: MarketMaker | null = null;
  const traders: Trader[] = [];

  let faucetAddr: string | null = null;
  try {
    faucetAddr = makeWallet(http, faucetKey(cfg).key).address;
  } catch {
    faucetAddr = null;
  }
  const mmWallet = cfg.mm.enabled ? makeWallet(http, mmKey(cfg).key) : null;
  if (mmWallet && faucetAddr && mmWallet.address.toLowerCase() === faucetAddr.toLowerCase()) {
    throw new Error("the market maker and the faucet must use different keys");
  }
  if (cfg.bootstrapPurr && (cfg.mm.enabled || cfg.faucet.enabled)) {
    await bootstrapPurr(cfg, mmWallet?.address ?? null, cfg.faucet.enabled ? faucetAddr : null).catch((e) =>
      log.warn(`PURR bootstrap failed: ${errText(e)}`),
    );
  }

  if (cfg.faucet.enabled) faucet = await startFaucet(cfg);

  if (mmWallet) {
    const want = (m: Market) =>
      m.kind === "spot" ? cfg.mm.spot && m.coin === "PURR/USDC" : cfg.mm.perps === "all" || cfg.mm.perps.includes(m.coin);
    mm = new MarketMaker(cfg.mm, info, mmWallet, markets.filter(want), faucetAddr);
    loops.push(mm.run(ac.signal).catch((e) => logger("mm").error(`stopped: ${errText(e)}`)));
  }

  if (cfg.traders.enabled && cfg.traders.count > 0) {
    const names = Array.from({ length: cfg.traders.count }, (_, i) => {
      const p = PERSONALITIES[i % PERSONALITIES.length];
      return i < PERSONALITIES.length ? p : `${p}${Math.floor(i / PERSONALITIES.length) + 1}`;
    });
    const keys = botWallets(cfg.botsDir, names);
    const token = faucetToken(cfg.logDir, false);
    const tm = markets.filter((m) => m.kind === "spot" || cfg.traders.perps === "all" || cfg.traders.perps.includes(m.coin));
    names.forEach((n, i) => {
      const t = new Trader(n, PERSONALITIES[i % PERSONALITIES.length], cfg.traders, info, makeWallet(http, keys[n]), tm, cfg.faucetUrl, token);
      traders.push(t);
      loops.push(t.run(ac.signal).catch((e) => t.log.error(`stopped: ${errText(e)}`)));
    });
  }

  let stopping = false;
  const stop = async (sig: string) => {
    if (stopping) return;
    stopping = true;
    log.info(`${sig}: shutting down`);
    const hard = setTimeout(() => {
      log.warn("shutdown took too long, exiting");
      process.exit(1);
    }, 20_000);
    ac.abort();
    await Promise.race([Promise.allSettled(loops), new Promise((r) => setTimeout(r, 16_000))]);
    await Promise.allSettled([mm?.cancelAll(), ...traders.map((t) => t.cancelAll())]);
    for (const t of traders) {
      const s = t.stats;
      log.info(`${t.name}: ${s.actions} steps, ${s.fills} fills, ${s.topUps} faucet top-ups, ${s.liquidations} liquidations, ${s.errors} errors`);
    }
    if (mm) log.info(`mm: ${mm.stats.ticks} ticks, ${mm.stats.requotes} requotes, ${mm.stats.errors} errors`);
    await faucet?.close();
    clearTimeout(hard);
    log.info("stopped");
    process.exit(0);
  };
  process.on("SIGINT", () => void stop("SIGINT"));
  process.on("SIGTERM", () => void stop("SIGTERM"));
  process.on("unhandledRejection", (e) => log.error(`unhandled: ${errText(e)}`));
}

main().catch((e) => {
  log.error(errText(e));
  process.exit(1);
});
