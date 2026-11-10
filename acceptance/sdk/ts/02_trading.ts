// AC5 TS example 02: README.md "Quick Example / Trading", verbatim except:
//   privateKeyToAccount("0x...")  ->  privateKeyToAccount(EXAMPLE_KEY)
//   new HttpTransport()           ->  new HttpTransport({ apiUrl: "http://127.0.0.1:3001", isTestnet: true })
//   destination: "0x..."          ->  destination: EXAMPLE_ADDRESS
// (asset 0 = BTC, mid ~60000 on the localnet: the README's 95000 Gtc buy crosses the
// seeded ask ladder and fills, as on HL when the price is above the best ask.)
import { EXAMPLE_ADDRESS, EXAMPLE_KEY } from "./_harness.mjs";
// --- BEGIN verbatim README.md "Trading"
// 1. Import modules
import { ExchangeClient, HttpTransport } from "@nktkas/hyperliquid";
import { privateKeyToAccount } from "viem/accounts";

// 2. Set up client with wallet and transport
const wallet = privateKeyToAccount(EXAMPLE_KEY);

const transport = new HttpTransport({ apiUrl: "http://127.0.0.1:3001", isTestnet: true });
const exchange = new ExchangeClient({ transport, wallet });

// 3. Execute an action

// Place an order
const result = await exchange.order({
  orders: [{
    a: 0,
    b: true,
    p: "95000",
    s: "0.01",
    r: false,
    t: { limit: { tif: "Gtc" } },
  }],
  grouping: "na",
});

// Update leverage
await exchange.updateLeverage({ asset: 0, isCross: true, leverage: 5 });

// Initiate a withdrawal request
await exchange.withdraw3({ destination: EXAMPLE_ADDRESS, amount: "1" });
// --- END verbatim
console.log("order result", JSON.stringify(result));
