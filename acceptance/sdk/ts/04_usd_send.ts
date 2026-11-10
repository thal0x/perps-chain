// AC5 TS example 04: API reference example (JSDoc in src/api/exchange/client.ts) for usdSend.
// Verbatim except:
//   import ... from "npm:viem/accounts"  ->  "viem/accounts"
//   privateKeyToAccount("0x...")          ->  privateKeyToAccount(EXAMPLE_KEY)
//   new hl.HttpTransport()                ->  new hl.HttpTransport({ apiUrl: "http://127.0.0.1:3001", isTestnet: true })
//   destination: "0x..."                  ->  destination: "0x0000000000000000000000000000000000000000"
//                                            (as the Python basic_transfer example)
import { EXAMPLE_KEY } from "./_harness.mjs";
// --- BEGIN verbatim client.ts ExchangeClient.usdSend
import * as hl from "@nktkas/hyperliquid";
import { privateKeyToAccount } from "viem/accounts";

const wallet = privateKeyToAccount(EXAMPLE_KEY); // viem or ethers
const transport = new hl.HttpTransport({ apiUrl: "http://127.0.0.1:3001", isTestnet: true }); // or `WebSocketTransport`
const client = new hl.ExchangeClient({ transport, wallet });

await client.usdSend({ destination: "0x0000000000000000000000000000000000000000", amount: "1" });
// --- END verbatim
