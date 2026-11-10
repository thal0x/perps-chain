// AC5 TS example 05: docs/guides/market-orders.md "Example", verbatim except:
//   privateKeyToAccount("0x...")  ->  privateKeyToAccount(EXAMPLE_KEY)
//   new HttpTransport()           ->  new HttpTransport({ apiUrl: "http://127.0.0.1:3001", isTestnet: true })
// run_ts.sh runs it while 06_subscriptions.ts is subscribed (the fill drives
// the userFills / orderUpdates channels).
import { EXAMPLE_KEY } from "./_harness.mjs";
// --- BEGIN verbatim docs/guides/market-orders.md
import { ExchangeClient, HttpTransport, InfoClient } from "@nktkas/hyperliquid";
import { formatPrice, formatSize, SymbolConverter } from "@nktkas/hyperliquid/utils";
import { privateKeyToAccount } from "viem/accounts";

const wallet = privateKeyToAccount(EXAMPLE_KEY);

const transport = new HttpTransport({ apiUrl: "http://127.0.0.1:3001", isTestnet: true });
const converter = await SymbolConverter.create({ transport });
const info = new InfoClient({ transport });
const exchange = new ExchangeClient({ transport, wallet });

// Parameters
const coin = "ETH";
const size = "0.1";
const isBuy = true;
const tolerance = 0.01; // 1% price buffer

// Get aggressive price based on current mid price with tolerance
const mids = await info.allMids();
const mid = parseFloat(mids[coin]);
const price = mid * (1 + (isBuy ? tolerance : -tolerance));

// `!` asserts the symbol exists — in production, handle `undefined` explicitly
const assetId = converter.getAssetId(coin)!;
const szDecimals = converter.getSzDecimals(coin)!;

// Place IoC order with aggressive price to ensure fill
await exchange.order({
  orders: [{
    a: assetId,
    b: isBuy,
    p: formatPrice(price, szDecimals),
    s: formatSize(size, szDecimals),
    r: false,
    t: { limit: { tif: "Ioc" } },
  }],
  grouping: "na",
});
// --- END verbatim
