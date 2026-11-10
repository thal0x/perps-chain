// AC5 TS example 06 (driver, glue): subscribes via three verbatim snippet modules, then
// hold(WS_SECS) keeps them open (>= 30 s) and fails the script if any subscription got
// no event. run_ts.sh runs 03_order_modify_cancel.ts and 05_market_order.ts meanwhile.
//   06a_realtime.ts     README.md "Real-time updates" (allMids, openOrders, l2Book)
//   06b_user_fills.ts   client.ts SubscriptionClient.userFills @example
//   06c_order_updates.ts client.ts SubscriptionClient.orderUpdates @example
import { hold } from "./_harness.mjs";
await import("./06a_realtime.ts");
await import("./06b_user_fills.ts");
await import("./06c_order_updates.ts");
await hold(Number(process.env.WS_SECS ?? 40));
