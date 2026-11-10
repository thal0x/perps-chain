// AC5 TS example 03c: client.ts ExchangeClient.cancel @example.
// Verbatim except:
//   import ... from "npm:viem/accounts"  ->  "viem/accounts"   (Deno specifier -> Node)
//   privateKeyToAccount("0x...")          ->  privateKeyToAccount(EXAMPLE_KEY)
//   new hl.HttpTransport()                ->  new hl.HttpTransport({ apiUrl: "http://127.0.0.1:3001", isTestnet: true })
//   o: 123                                ->  o: OID   (the open, modified order)
import { EXAMPLE_KEY, OID } from "./_harness.mjs";
// --- BEGIN verbatim client.ts ExchangeClient.cancel
import * as hl from "@nktkas/hyperliquid";
import { privateKeyToAccount } from "viem/accounts";

const wallet = privateKeyToAccount(EXAMPLE_KEY); // viem or ethers
const transport = new hl.HttpTransport({ apiUrl: "http://127.0.0.1:3001", isTestnet: true }); // or `WebSocketTransport`
const client = new hl.ExchangeClient({ transport, wallet });

await client.cancel({ cancels: [{ a: 0, o: OID }] });
// --- END verbatim
