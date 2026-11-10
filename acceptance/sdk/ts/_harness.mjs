// AC5 (TypeScript) observation harness. Imported first by every example script;
// it is NOT part of the examples. It only observes:
//  * wraps every ExchangeClient / InfoClient / SubscriptionClient prototype method
//    to log each call (params, result or error) as an `ACC_CALL {json}` line;
//  * counts events delivered to each subscription listener;
//  * on exit prints `ACC_RESULT {json}` with the verdict:
//      - every ExchangeClient call succeeded (the SDK throws ApiRequestError on
//        {"status":"err"} and on any per-order {"error"}) unless the exact error is
//        in acceptance/sdk/expected_errors.json for this script (D23), and every
//        allowlisted error for this script occurred;
//      - successful exchange responses have HL's response type for the action
//        (order->"order", cancel->"cancel", modify/updateLeverage/usdSend/withdraw3->"default");
//      - every subscription received >= 1 event (scripts that call hold()).
// Env (set by run_ts.sh): HL_ACC_KEY, HL_ACC_ADDR, HL_ACC_ALLOWLIST, HL_ACC_SCRIPT.
import { ExchangeClient, InfoClient, SubscriptionClient } from "@nktkas/hyperliquid";
import { readFileSync } from "node:fs";

export const EXAMPLE_KEY = process.env.HL_ACC_KEY;
export const EXAMPLE_ADDRESS = process.env.HL_ACC_ADDR;
if (!EXAMPLE_KEY || !EXAMPLE_ADDRESS) throw new Error("HL_ACC_KEY / HL_ACC_ADDR not set (run via run_ts.sh)");

const SCRIPT = process.env.HL_ACC_SCRIPT ?? "unknown";
const allow = (() => {
  try {
    return JSON.parse(readFileSync(process.env.HL_ACC_ALLOWLIST, "utf8")).filter((e) => e.script === SCRIPT);
  } catch {
    return [];
  }
})();
const matched = new Set();
const calls = [];
const reasons = [];
const subCounts = {}; // "method params" -> events
let holding = false;

const EXPECTED_TYPE = {
  order: "order",
  cancel: "cancel",
  cancelByCloid: "cancel",
  modify: "default",
  updateLeverage: "default",
  usdSend: "default",
  withdraw3: "default",
};

const json = (v) => JSON.stringify(v, (_k, x) => (typeof x === "bigint" ? x.toString() : x));

function wrap(Cls, kind) {
  for (const name of Object.getOwnPropertyNames(Cls.prototype)) {
    if (name === "constructor") continue;
    const d = Object.getOwnPropertyDescriptor(Cls.prototype, name);
    if (!d || typeof d.value !== "function") continue;
    const orig = d.value;
    Cls.prototype[name] = async function (...args) {
      const call = `${Cls.name}.${name}`;
      let subKey = null;
      if (kind === "sub") {
        const i = args.findIndex((a) => typeof a === "function");
        if (i >= 0) {
          const params = i > 0 ? args[0] : {};
          subKey = `${name} ${json(params)}`;
          subCounts[subKey] = 0;
          const listener = args[i];
          args[i] = (data) => {
            subCounts[subKey]++;
            return listener(data);
          };
        }
      }
      const rec = { call, params: args.filter((a) => typeof a !== "function")[0] };
      try {
        const res = await orig.apply(this, args);
        if (kind !== "sub") rec.result = res;
        else rec.subscribed = subKey;
        if (kind === "exchange" && EXPECTED_TYPE[name] && res?.response?.type !== EXPECTED_TYPE[name]) {
          reasons.push(`${call}: response type ${json(res?.response?.type)} != HL's ${json(EXPECTED_TYPE[name])}`);
        }
        console.log("ACC_CALL " + json(rec));
        calls.push(rec);
        return res;
      } catch (e) {
        rec.error = `${e?.name}: ${e?.message}`;
        const hit = allow.findIndex((a) => a.call === call && a.error_text === e?.message);
        if (hit >= 0) {
          matched.add(hit);
          rec.allowlisted = true;
        } else {
          reasons.push(`${call} failed: ${rec.error}`);
        }
        if (e?.response) rec.response = e.response;
        console.log("ACC_CALL " + json(rec));
        calls.push(rec);
        throw e;
      }
    };
  }
}
wrap(ExchangeClient, "exchange");
wrap(InfoClient, "info");
wrap(SubscriptionClient, "sub");

/** Keep subscriptions open for `seconds`, then judge channel receipt and exit. */
export async function hold(seconds) {
  holding = true;
  console.log("ACC_SUBSCRIBED " + json(Object.keys(subCounts)));
  await new Promise((r) => setTimeout(r, seconds * 1000));
  for (const [k, n] of Object.entries(subCounts)) if (n === 0) reasons.push(`no event on subscription ${k}`);
  if (Object.keys(subCounts).length === 0) reasons.push("no subscriptions");
  process.exit(0);
}

process.on("exit", (code) => {
  allow.forEach((a, i) => {
    if (!matched.has(i)) reasons.push(`allowlisted error did not occur: ${a.call}: ${a.error_text}`);
  });
  if (code !== 0) reasons.push(`exit code ${code}`);
  const res = {
    script: SCRIPT,
    pass: reasons.length === 0,
    reasons,
    exchange_calls: calls.filter((c) => c.call.startsWith("ExchangeClient.")).length,
    info_calls: calls.filter((c) => c.call.startsWith("InfoClient.")).length,
    subscriptions: holding ? subCounts : undefined,
    calls: calls.map((c) => ({ call: c.call, ok: !c.error, error: c.error, allowlisted: c.allowlisted })),
  };
  console.log("ACC_RESULT " + json(res));
});

// Shared glue state between the split snippet modules (ESM live binding).
export let OID;
export function setOid(x) {
  OID = x;
}
