// AC5 TS example 06a: README.md "Real-time updates", verbatim except:
//   new WebSocketTransport()  ->  new WebSocketTransport({ url: "ws://127.0.0.1:3001/ws", isTestnet: true })
//   user: "0x..."             ->  user: EXAMPLE_ADDRESS
import { EXAMPLE_ADDRESS } from "./_harness.mjs";
// --- BEGIN verbatim README.md "Real-time updates"
// 1. Import module
import { SubscriptionClient, WebSocketTransport } from "@nktkas/hyperliquid";

// 2. Set up client with transport
const transport = new WebSocketTransport({ url: "ws://127.0.0.1:3001/ws", isTestnet: true });
const subs = new SubscriptionClient({ transport });

// 3. Subscribe to events

// Subscribe to mids for all coins
await subs.allMids((data) => {
  console.log(data);
});

// Subscribe to user's open orders
await subs.openOrders({ user: EXAMPLE_ADDRESS }, (data) => {
  console.log(data);
});

// Subscribe to L2 book snapshot
await subs.l2Book({ coin: "ETH" }, (data) => {
  console.log(data);
});
// --- END verbatim
