// AC5 TS example 03b: client.ts ExchangeClient.modify @example.
// Verbatim except:
//   import ... from "npm:viem/accounts"  ->  "viem/accounts"   (Deno specifier -> Node)
//   privateKeyToAccount("0x...")          ->  privateKeyToAccount(EXAMPLE_KEY)
//   new hl.HttpTransport()                ->  new hl.HttpTransport({ apiUrl: "http://127.0.0.1:3001", isTestnet: true })
//   oid: 123                              ->  oid: OID   (the order 03a_order.ts placed)
import { EXAMPLE_KEY, OID } from "./_harness.mjs";
// --- BEGIN verbatim client.ts ExchangeClient.modify
import * as hl from "@nktkas/hyperliquid";
import { privateKeyToAccount } from "viem/accounts";

const wallet = privateKeyToAccount(EXAMPLE_KEY); // viem or ethers
const transport = new hl.HttpTransport({ apiUrl: "http://127.0.0.1:3001", isTestnet: true }); // or `WebSocketTransport`
const client = new hl.ExchangeClient({ transport, wallet });

await client.modify({
  oid: OID,
  order: {
    a: 0,
    b: true,
    p: "31000",
    s: "0.2",
    r: false,
    t: { limit: { tif: "Gtc" } },
  },
});
// --- END verbatim
