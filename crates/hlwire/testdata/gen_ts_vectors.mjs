// Generate golden HL wire vectors with the TS SDK `@nktkas/hyperliquid`.
//
// Usage:
//   mkdir /some/scratch && cd /some/scratch && npm install @nktkas/hyperliquid viem
//   HLTS_DIR=/some/scratch node gen_ts_vectors.mjs > ts_vectors.json
//
// Every vector is produced by calling the real `ExchangeClient` method with a
// custom transport that captures the request payload instead of sending it.
// The captured payload is serialized with JSON.stringify exactly like the
// SDK's HttpTransport does. For each request we record the msgpack bytes the
// SDK hashes (same `adjust` + @std/msgpack encode as signing/_l1.ts), the
// action hash (createL1ActionHash), the EIP-712 digest (viem hashTypedData)
// and the expected signer.

import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import path from "node:path";

const dir = process.env.HLTS_DIR || "/home/user/.hlts";
const req = createRequire(path.join(dir, "package.json"));
const imp = (p) => import(pathToFileURL(p).href);

const hl = await imp(req.resolve("@nktkas/hyperliquid"));
const signing = await imp(req.resolve("@nktkas/hyperliquid/signing"));
const exch = await imp(req.resolve("@nktkas/hyperliquid/api/exchange"));
const viem = await imp(req.resolve("viem"));
const accounts = await imp(req.resolve("viem/accounts"));
const hlRoot = path.dirname(req.resolve("@nktkas/hyperliquid"));
const { encode } = await imp(path.join(hlRoot, "_deps/jsr.io/@std/msgpack/1.0.3/encode.js"));

const USER_KEY = "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318";
const AGENT_KEY = "0x" + "11".repeat(32);
const VAULT = "0x1719884eb866cb12b2287399b15f7db5e7d775ea";
const DEST = "0x5e9ee1089755c3435139848e47e6635505d5a13a";

// copy of `adjust` from signing/_l1.ts (not exported)
function adjust(value) {
  if (Array.isArray(value)) return value.map(adjust);
  if (typeof value === "object" && value !== null) {
    const result = {};
    for (const key in value) {
      const entry = value[key];
      if (entry !== undefined) result[key] = adjust(entry);
    }
    return result;
  }
  if (typeof value === "number" && Number.isInteger(value) && (value >= 0x100000000 || value < -0x80000000)) {
    return BigInt(value);
  }
  return value;
}

const USER_TYPES = {
  usdSend: exch.UsdSendTypes,
  spotSend: exch.SpotSendTypes,
  withdraw3: exch.Withdraw3Types,
  usdClassTransfer: exch.UsdClassTransferTypes,
  approveAgent: exch.ApproveAgentTypes,
};

let captured = [];
let nonce = 1_700_000_000_000;
const nonceManager = () => ++nonce;

function transport(isTestnet) {
  return {
    isTestnet,
    async request(endpoint, payload) {
      captured.push(JSON.stringify(payload));
      return { status: "ok", response: { type: "default" } };
    },
  };
}

function client(key, isTestnet, extra = {}) {
  return new hl.ExchangeClient({
    transport: transport(isTestnet),
    wallet: accounts.privateKeyToAccount(key),
    nonceManager,
    ...extra,
  });
}

const vectors = [];

async function record(name, key, isTestnet, fn) {
  captured = [];
  await fn();
  if (captured.length !== 1) throw new Error(`${name}: expected 1 request, got ${captured.length}`);
  const bodyRaw = captured[0];
  const body = JSON.parse(bodyRaw);
  const action = body.action;
  const signer = accounts.privateKeyToAccount(key).address.toLowerCase();
  const v = {
    name,
    sdk: "ts",
    is_mainnet: !isTestnet,
    signer,
    body_raw: bodyRaw,
    body,
    msgpack_hex: Buffer.from(encode(adjust(action))).toString("hex"),
  };
  let digest;
  if (USER_TYPES[action.type]) {
    const types = USER_TYPES[action.type];
    const primaryType = Object.keys(types)[0];
    digest = viem.hashTypedData({
      domain: {
        name: "HyperliquidSignTransaction",
        version: "1",
        chainId: parseInt(action.signatureChainId),
        verifyingContract: "0x0000000000000000000000000000000000000000",
      },
      types,
      primaryType,
      message: action,
    });
    v.kind = "user";
    v.primary_type = primaryType;
  } else {
    const h = signing.createL1ActionHash({
      action,
      nonce: body.nonce,
      vaultAddress: body.vaultAddress,
      expiresAfter: body.expiresAfter,
    });
    digest = viem.hashTypedData({
      domain: { name: "Exchange", version: "1", chainId: 1337, verifyingContract: "0x0000000000000000000000000000000000000000" },
      types: { Agent: [{ name: "source", type: "string" }, { name: "connectionId", type: "bytes32" }] },
      primaryType: "Agent",
      message: { source: isTestnet ? "b" : "a", connectionId: h },
    });
    v.kind = "l1";
    v.action_hash = h;
  }
  v.digest = digest;
  const sig = viem.serializeSignature({ r: body.signature.r, s: body.signature.s, v: BigInt(body.signature.v) });
  const rec = (await viem.recoverAddress({ hash: digest, signature: sig })).toLowerCase();
  if (rec !== signer) throw new Error(`${name}: recovered ${rec} != ${signer}`);
  vectors.push(v);
}

for (const isTestnet of [true, false]) {
  const sfx = isTestnet ? "" : "_mainnet";
  const c = client(USER_KEY, isTestnet);
  await record("order_limit_gtc" + sfx, USER_KEY, isTestnet, () =>
    c.order({ orders: [{ a: 0, b: true, p: "30000.5", s: "0.02", r: false, t: { limit: { tif: "Gtc" } } }], grouping: "na" }));
  await record("usd_send" + sfx, USER_KEY, isTestnet, () => c.usdSend({ destination: DEST, amount: "12.5" }));
}

const c = client(USER_KEY, true);
await record("order_numbers_normalized", USER_KEY, true, () =>
  c.order({ orders: [{ a: 1, b: false, p: 1100.0, s: "1.23450", r: true, t: { limit: { tif: "Ioc" } } }], grouping: "na" }));
await record("order_cloid_upper", USER_KEY, true, () =>
  c.order({ orders: [{ a: 2, b: true, p: "0.0001234", s: "3", r: false, t: { limit: { tif: "Alo" } }, c: "0x00000000000000000000000000ABCDEF" }], grouping: "na" }));
await record("order_frontend_market", USER_KEY, true, () =>
  c.order({ orders: [{ a: 0, b: true, p: "70000", s: "0.001", r: false, t: { limit: { tif: "FrontendMarket" } } }], grouping: "na" }));
await record("order_trigger_sl_market", USER_KEY, true, () =>
  c.order({ orders: [{ a: 0, b: false, p: "29000", s: "0.5", r: true, t: { trigger: { isMarket: true, triggerPx: "29500", tpsl: "sl" } } }], grouping: "na" }));
await record("order_trigger_key_order", USER_KEY, true, () =>
  // keys given out of schema order: SDK canonicalizes
  c.order({ grouping: "na", orders: [{ t: { trigger: { tpsl: "tp", triggerPx: "2000", isMarket: false } }, r: false, s: "2", p: "1999.9", b: true, a: 1 }] }));
await record("order_builder", USER_KEY, true, () =>
  c.order({ orders: [{ a: 0, b: true, p: "40000", s: "0.01", r: false, t: { limit: { tif: "Gtc" } } }], grouping: "na", builder: { b: "0x8C967E73E7B15087C42A10D344CFF4C96D877F1D", f: 10 } }));
await record("order_grouping_default", USER_KEY, true, () =>
  c.order({ orders: [{ a: 0, b: true, p: "40000", s: "0.01", r: false, t: { limit: { tif: "Gtc" } } }] }));
await record("order_grouping_priority", USER_KEY, true, () =>
  c.order({ orders: [{ a: 0, b: true, p: "40000", s: "0.01", r: false, t: { limit: { tif: "Ioc" } } }], grouping: { p: 1000 } }));
await record("bulk_orders_normal_tpsl", USER_KEY, true, () =>
  c.order({
    orders: [
      { a: 0, b: true, p: "50000", s: "0.1", r: false, t: { limit: { tif: "Gtc" } } },
      { a: 0, b: false, p: "60000", s: "0.1", r: true, t: { trigger: { isMarket: true, triggerPx: "60000", tpsl: "tp" } } },
      { a: 0, b: false, p: "45000", s: "0.1", r: true, t: { trigger: { isMarket: true, triggerPx: "45000", tpsl: "sl" } } },
    ],
    grouping: "normalTpsl",
  }));
await record("bulk_orders_position_tpsl", USER_KEY, true, () =>
  c.order({ orders: [{ a: 1, b: false, p: "4000", s: "0", r: true, t: { trigger: { isMarket: true, triggerPx: "4000", tpsl: "tp" } } }], grouping: "positionTpsl" }));
await record("bulk_orders_50", USER_KEY, true, () =>
  c.order({
    orders: Array.from({ length: 50 }, (_, i) => ({
      a: 0, b: i % 2 === 0, p: String(30000 + i), s: (0.001 * (i + 1)).toFixed(3), r: false, t: { limit: { tif: "Alo" } },
      c: "0x" + (1000 + i).toString(16).padStart(32, "0"),
    })),
    grouping: "na",
  }));
await record("cancel", USER_KEY, true, () => c.cancel({ cancels: [{ a: 0, o: 123456789 }] }));
await record("cancel_big_oid_fast", USER_KEY, true, () => c.cancel({ cancels: [{ a: 0, o: 1 }, { a: 2, o: 2 ** 40 + 7 }], f: true }));
await record("cancel_by_cloid", USER_KEY, true, () => c.cancelByCloid({ cancels: [{ asset: 1, cloid: "0x000000000000000000000000DEADBEEF" }] }));
await record("modify_by_oid", USER_KEY, true, () =>
  c.modify({ oid: 77, order: { a: 0, b: true, p: "31000", s: "0.03", r: false, t: { limit: { tif: "Gtc" } } } }));
await record("modify_by_cloid_allow", USER_KEY, true, () =>
  c.modify({ oid: "0x00000000000000000000000000000005", order: { a: 0, b: false, p: "31000", s: "0.03", r: false, t: { limit: { tif: "Alo" } }, c: "0x00000000000000000000000000000006" }, a: true }));
await record("modify_by_big_cloid", USER_KEY, true, () =>
  // a full 128-bit cloid is not a safe integer, so it stays a string
  c.modify({ oid: "0xd4c3b2a1f0e9d8c7b6a5948372615049", order: { a: 0, b: true, p: "31000", s: "0.03", r: false, t: { limit: { tif: "Gtc" } } } }));
await record("batch_modify", USER_KEY, true, () =>
  c.batchModify({
    modifies: [
      { oid: 1, order: { a: 0, b: true, p: "10", s: "1", r: false, t: { limit: { tif: "Gtc" } } } },
      { oid: "0x00000000000000000000000000000002", order: { a: 1, b: false, p: "3000.25", s: "0.5", r: false, t: { limit: { tif: "Gtc" } }, c: "0x00000000000000000000000000000002" } },
    ],
  }));
await record("schedule_cancel_time", USER_KEY, true, () => c.scheduleCancel({ time: 1_700_000_100_000 }));
await record("schedule_cancel_none", USER_KEY, true, () => c.scheduleCancel());
await record("update_leverage_cross", USER_KEY, true, () => c.updateLeverage({ asset: 0, isCross: true, leverage: 20 }));
await record("update_leverage_isolated", USER_KEY, true, () => c.updateLeverage({ asset: 2, isCross: false, leverage: 3 }));
await record("update_isolated_margin_add", USER_KEY, true, () => c.updateIsolatedMargin({ asset: 1, isBuy: true, ntli: 12_500_000 }));
await record("update_isolated_margin_remove", USER_KEY, true, () => c.updateIsolatedMargin({ asset: 1, isBuy: false, ntli: -3_000_250_000 }));
await record("update_isolated_margin_small_neg", USER_KEY, true, () => c.updateIsolatedMargin({ asset: 1, isBuy: true, ntli: -100 }));
await record("set_referrer", USER_KEY, true, () => c.setReferrer({ code: "ASDFASDF" }));
await record("noop", USER_KEY, true, () => c.noop({ nonce: 1_700_000_555_555 }));
await record("usd_send_upper_dest", USER_KEY, true, () => c.usdSend({ destination: DEST.toUpperCase().replace("0X", "0x"), amount: "5" }));
await record("spot_send", USER_KEY, true, () => c.spotSend({ destination: DEST, token: "PURR:0xc4bf3f870c0e9465323c0b6ed28096c2", amount: "1.25" }));
await record("withdraw3", USER_KEY, true, () => c.withdraw3({ destination: DEST, amount: "2" }));
await record("usd_class_transfer_to_perp", USER_KEY, true, () => c.usdClassTransfer({ amount: "10", toPerp: true }));
await record("usd_class_transfer_suffix", USER_KEY, true, () => c.usdClassTransfer({ amount: `1 subaccount:${VAULT}`, toPerp: false }));
await record("approve_agent_named", USER_KEY, true, () => c.approveAgent({ agentAddress: "0x1563915E194D8CfBA1943570603F7606A3115508", agentName: "bot1" }));
await record("approve_agent_unnamed", USER_KEY, true, () => c.approveAgent({ agentAddress: "0x1563915E194D8CfBA1943570603F7606A3115508" }));
await record("approve_agent_null", USER_KEY, true, () => c.approveAgent({ agentAddress: "0x1563915E194D8CfBA1943570603F7606A3115508", agentName: null }));

// explicit signatureChainId (SDK default for viem local accounts is 0x1)
const c66 = client(USER_KEY, true, { signatureChainId: "0x66eee" });
await record("usd_send_chain_66eee", USER_KEY, true, () => c66.usdSend({ destination: DEST, amount: "0.000001" }));
await record("withdraw3_chain_66eee", USER_KEY, true, () => c66.withdraw3({ destination: DEST, amount: "1000000" }));
const cm = client(USER_KEY, false, { signatureChainId: "0xa4b1" });
await record("approve_agent_mainnet_arb", USER_KEY, false, () => cm.approveAgent({ agentAddress: DEST, agentName: "m" }));

// vault + expiresAfter
await record("order_vault", USER_KEY, true, () =>
  c.order({ orders: [{ a: 0, b: true, p: "30000", s: "0.02", r: false, t: { limit: { tif: "Gtc" } } }], grouping: "na" }, { vaultAddress: VAULT }));
await record("cancel_vault", USER_KEY, true, () => c.cancel({ cancels: [{ a: 0, o: 9 }] }, { vaultAddress: VAULT }));
await record("order_expires", USER_KEY, true, () =>
  c.order({ orders: [{ a: 1, b: false, p: "2500", s: "0.1", r: false, t: { limit: { tif: "Ioc" } } }], grouping: "na" }, { expiresAfter: 1_800_000_000_000 }));
const cve = client(USER_KEY, false, { defaultVaultAddress: VAULT, defaultExpiresAfter: 1_800_000_000_001 });
await record("order_vault_expires_mainnet", USER_KEY, false, () =>
  cve.order({ orders: [{ a: 2, b: true, p: "150.5", s: "10", r: false, t: { limit: { tif: "Gtc" } } }], grouping: "na" }));
await record("update_leverage_vault_expires_mainnet", USER_KEY, false, () => cve.updateLeverage({ asset: 1, isCross: false, leverage: 5 }));

// agent signing
const ca = client(AGENT_KEY, true);
await record("agent_order", AGENT_KEY, true, () =>
  ca.order({ orders: [{ a: 0, b: false, p: "35000", s: "0.05", r: false, t: { limit: { tif: "Gtc" } } }], grouping: "na" }));
await record("agent_cancel", AGENT_KEY, true, () => ca.cancel({ cancels: [{ a: 0, o: 42 }] }));

process.stdout.write(JSON.stringify(vectors, null, 1) + "\n");
