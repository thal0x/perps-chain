import {
  ExchangeClient,
  HttpTransport,
  InfoClient,
  SubscriptionClient,
  WebSocketTransport,
} from "@nktkas/hyperliquid";
import { privateKeyToAccount, type PrivateKeyAccount } from "viem/accounts";
import { wsUrlFor } from "./settings";

export interface Clients {
  apiUrl: string;
  http: HttpTransport;
  ws: WebSocketTransport;
  info: InfoClient;
  subs: SubscriptionClient;
}

/** One HTTP + one WS transport per API URL. The local chain signs as testnet (D9). */
export function makeClients(apiUrl: string): Clients {
  const http = new HttpTransport({ apiUrl, isTestnet: true, timeout: 15_000 });
  const ws = new WebSocketTransport({ url: wsUrlFor(apiUrl), isTestnet: true, timeout: 15_000 });
  return { apiUrl, http, ws, info: new InfoClient({ transport: http }), subs: new SubscriptionClient({ transport: ws }) };
}

const cache = new Map<string, Clients>();

/** Cached per URL so React StrictMode re-mounts don't close a live socket. */
export function getClients(apiUrl: string): Clients {
  let c = cache.get(apiUrl);
  if (!c) {
    c = makeClients(apiUrl);
    cache.set(apiUrl, c);
  }
  return c;
}

/** Close transports for URLs no longer in use. */
export function closeOtherClients(apiUrl: string) {
  for (const [u, c] of cache) {
    if (u === apiUrl) continue;
    cache.delete(u);
    try {
      c.ws.close();
    } catch {
      /* already closed */
    }
  }
}

export interface Signer {
  account: PrivateKeyAccount;
  address: `0x${string}`;
  exchange: ExchangeClient;
}

export function makeSigner(http: HttpTransport, key: `0x${string}`): Signer {
  const account = privateKeyToAccount(key);
  return { account, address: account.address, exchange: new ExchangeClient({ transport: http, wallet: account }) };
}
