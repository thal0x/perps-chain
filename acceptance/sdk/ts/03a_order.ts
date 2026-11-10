// AC5 TS example 03a: client.ts ExchangeClient.order @example.
// Verbatim except:
//   import ... from "npm:viem/accounts"  ->  "viem/accounts"   (Deno specifier -> Node)
//   privateKeyToAccount("0x...")          ->  privateKeyToAccount(EXAMPLE_KEY)
//   new hl.HttpTransport()                ->  new hl.HttpTransport({ apiUrl: "http://127.0.0.1:3001", isTestnet: true })
import { EXAMPLE_KEY, setOid } from "./_harness.mjs";
// --- BEGIN verbatim client.ts ExchangeClient.order
import * as hl from "@nktkas/hyperliquid";
import { privateKeyToAccount } from "viem/accounts";

const wallet = privateKeyToAccount(EXAMPLE_KEY); // viem or ethers
const transport = new hl.HttpTransport({ apiUrl: "http://127.0.0.1:3001", isTestnet: true }); // or `WebSocketTransport`
const client = new hl.ExchangeClient({ transport, wallet });

const data = await client.order({
  orders: [
    {
      a: 0,
      b: true,
      p: "30000",
      s: "0.1",
      r: false,
      t: { limit: { tif: "Gtc" } },
    },
  ],
  grouping: "na",
});
// --- END verbatim
console.log("order", JSON.stringify(data));
setOid(data.response.data.statuses[0].resting.oid);
