// AC5 TS example 06c: client.ts SubscriptionClient.orderUpdates @example, verbatim except:
//   new hl.WebSocketTransport()  ->  new hl.WebSocketTransport({ url: "ws://127.0.0.1:3001/ws", isTestnet: true })
//   user: "0x..."                ->  user: EXAMPLE_ADDRESS
import { EXAMPLE_ADDRESS } from "./_harness.mjs";
// --- BEGIN verbatim client.ts SubscriptionClient.orderUpdates
import * as hl from "@nktkas/hyperliquid";

const transport = new hl.WebSocketTransport({ url: "ws://127.0.0.1:3001/ws", isTestnet: true });
const client = new hl.SubscriptionClient({ transport });

const sub = await client.orderUpdates({ user: EXAMPLE_ADDRESS }, (data) => {
  console.log(data);
});
// --- END verbatim
