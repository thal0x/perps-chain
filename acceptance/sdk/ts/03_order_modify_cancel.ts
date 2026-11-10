// AC5 TS example 03: API reference examples (JSDoc @example in src/api/exchange/client.ts,
// the gitbook "Exchange methods" reference) for order -> modify -> cancel, one module per
// snippet (each declares wallet/transport/client): 03a_order.ts, 03b_modify.ts, 03c_cancel.ts.
// This driver is glue: it runs them in order and checks openOrders in between.
import { EXAMPLE_ADDRESS, OID, setOid } from "./_harness.mjs";
import * as hl from "@nktkas/hyperliquid";

const info = new hl.InfoClient({ transport: new hl.HttpTransport({ apiUrl: "http://127.0.0.1:3001", isTestnet: true }) });
await import("./03a_order.ts");
await import("./03b_modify.ts");
const oo = await info.openOrders({ user: EXAMPLE_ADDRESS });
console.log("openOrders after modify", JSON.stringify(oo));
const m = oo.filter((o) => o.coin === "BTC" && o.limitPx === "31000.0" && o.sz === "0.2");
if (m.length !== 1) throw new Error("modified order (BTC buy 0.2 @ 31000) not found exactly once in openOrders");
setOid(m[0].oid); // HL gives a modified order a new oid in some versions; cancel what is open
await import("./03c_cancel.ts");
const after = await info.openOrders({ user: EXAMPLE_ADDRESS });
if (after.some((o) => o.oid === OID)) throw new Error(`order ${OID} still open after cancel`);
console.log("cancelled", OID);
