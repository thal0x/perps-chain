// TS SDK (@nktkas/hyperliquid) smoke test: README examples against an hlbft
// API. Usage: node smoke.mjs <base_url> <user_address>
import { ExchangeClient, HttpTransport, InfoClient, SubscriptionClient, WebSocketTransport } from "@nktkas/hyperliquid";
import { generatePrivateKey, privateKeyToAccount } from "viem/accounts";

const [base, user] = process.argv.slice(2);
const wsUrl = base.replace(/^http/, "ws") + "/ws";
const fail = (m) => { console.error("FAIL", m); process.exit(1); };
const timeout = (p, ms, what) => Promise.race([p, new Promise((_, r) => setTimeout(() => r(new Error(`timeout: ${what}`)), ms))]);

try {
  const transport = new HttpTransport({ apiUrl: base });
  const info = new InfoClient({ transport });
  const mids = await info.allMids();
  if (!mids.ETH) fail("allMids");
  await info.openOrders({ user });
  const book = await info.l2Book({ coin: "ETH" });
  if (!book || book.coin !== "ETH") fail("l2Book");
  await info.l2Book({ coin: "ETH", nSigFigs: 2 });
  await info.clearinghouseState({ user });
  await info.spotClearinghouseState({ user });
  await info.meta();
  await info.metaAndAssetCtxs();
  await info.spotMeta();
  await info.frontendOpenOrders({ user });
  await info.userFills({ user });
  await info.orderStatus({ user, oid: 10 });
  await info.historicalOrders({ user });
  await info.candleSnapshot({ coin: "ETH", interval: "1m", startTime: 0 });
  await info.userFees({ user });
  console.log("info ok");

  const wallet = privateKeyToAccount(generatePrivateKey());
  const exchange = new ExchangeClient({ transport, wallet });
  const r = await exchange.order({
    orders: [{ a: 0, b: true, p: "95000", s: "0.01", r: false, t: { limit: { tif: "Gtc" } } }],
    grouping: "na",
  });
  const oid = r.response.data.statuses[0].resting.oid;
  await exchange.cancel({ cancels: [{ a: 0, o: oid }] });
  await exchange.modify({ oid, order: { a: 0, b: true, p: "95001", s: "0.01", r: false, t: { limit: { tif: "Gtc" } } } });
  await exchange.updateLeverage({ asset: 0, isCross: true, leverage: 5 });
  await exchange.withdraw3({ destination: wallet.address, amount: "1" });
  await exchange.usdSend({ destination: wallet.address, amount: "1" });
  await exchange.scheduleCancel({ time: Date.now() + 10_000 });
  console.log("exchange ok");

  const ws = new WebSocketTransport({ url: wsUrl });
  const subs = new SubscriptionClient({ transport: ws });
  const seen = new Set();
  const want = ["allMids", "openOrders", "l2Book", "userFills", "orderUpdates", "clearinghouseState", "bbo", "trades"];
  const mark = (k) => () => seen.add(k);
  await timeout(subs.allMids(mark("allMids")), 5000, "sub allMids");
  await timeout(subs.openOrders({ user }, mark("openOrders")), 5000, "sub openOrders");
  await timeout(subs.l2Book({ coin: "ETH" }, mark("l2Book")), 5000, "sub l2Book");
  await timeout(subs.userFills({ user }, mark("userFills")), 5000, "sub userFills");
  await timeout(subs.orderUpdates({ user }, mark("orderUpdates")), 5000, "sub orderUpdates");
  await timeout(subs.clearinghouseState({ user }, mark("clearinghouseState")), 5000, "sub clearinghouseState");
  await timeout(subs.bbo({ coin: "ETH" }, mark("bbo")), 5000, "sub bbo");
  await timeout(subs.trades({ coin: "ETH" }, mark("trades")), 5000, "sub trades");
  const s2 = await subs.candle({ coin: "ETH", interval: "1m" }, () => {});
  await s2.unsubscribe();
  // info + exchange over the websocket transport (post)
  const wsInfo = new InfoClient({ transport: ws });
  if (!(await wsInfo.allMids()).ETH) fail("ws post info");
  const wsEx = new ExchangeClient({ transport: ws, wallet });
  await wsEx.updateLeverage({ asset: 0, isCross: true, leverage: 3 });
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline && !want.every((k) => seen.has(k))) await new Promise((r) => setTimeout(r, 100));
  const missing = want.filter((k) => !seen.has(k));
  if (missing.length) fail(`no ws events for ${missing}`);
  console.log("ws ok");
  await ws.close();
  process.exit(0);
} catch (e) {
  console.error(e);
  process.exit(1);
}
