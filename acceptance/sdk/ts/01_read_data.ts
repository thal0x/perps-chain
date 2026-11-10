// AC5 TS example 01: README.md "Quick Example / Read data", verbatim except:
//   new HttpTransport()  ->  new HttpTransport({ apiUrl: "http://127.0.0.1:3001", isTestnet: true })
//   user: "0x..."        ->  user: EXAMPLE_ADDRESS   (SDK example account, run/localnet/accounts.json)
// Lines outside the BEGIN/END markers are harness glue (printing results).
import { EXAMPLE_ADDRESS } from "./_harness.mjs";
// --- BEGIN verbatim README.md "Read data"
// 1. Import module
import { HttpTransport, InfoClient } from "@nktkas/hyperliquid";

// 2. Set up client with transport
const transport = new HttpTransport({ apiUrl: "http://127.0.0.1:3001", isTestnet: true });
const info = new InfoClient({ transport });

// 3. Query data

// Retrieve mids for all coins
const mids = await info.allMids();

// Retrieve a user's open orders
const openOrders = await info.openOrders({ user: EXAMPLE_ADDRESS });

// L2 book snapshot
const book = await info.l2Book({ coin: "BTC" });
// --- END verbatim
console.log("mids", JSON.stringify(mids));
console.log("openOrders", JSON.stringify(openOrders));
console.log("book", JSON.stringify(book));
if (!mids.BTC || !Array.isArray(openOrders) || book?.levels?.length !== 2) throw new Error("unexpected info shapes");
