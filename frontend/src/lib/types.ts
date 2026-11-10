// Narrow wire shapes used by the UI (subset of the HL API responses).

export type MarketKind = "perp" | "spot";

export interface Market {
  kind: MarketKind;
  /** Coin name the API uses (e.g. "BTC", "PURR/USDC"). */
  coin: string;
  /** Asset id for /exchange (perp index, or 10000 + spot index). */
  asset: number;
  szDecimals: number;
  maxLeverage: number;
  onlyIsolated: boolean;
  base: string;
  quote: string;
}

export interface MarketCtx {
  markPx?: string;
  midPx?: string | null;
  oraclePx?: string;
  funding?: string;
  openInterest?: string;
  prevDayPx?: string;
  dayNtlVlm?: string;
  premium?: string | null;
}

export interface Level {
  px: string;
  sz: string;
  n: number;
}
export interface Book {
  coin: string;
  time: number;
  levels: [Level[], Level[]];
}

export interface Trade {
  coin: string;
  side: "B" | "A";
  px: string;
  sz: string;
  time: number;
  tid: number;
  hash?: string;
}

export interface Candle {
  t: number;
  T: number;
  s: string;
  i: string;
  o: string;
  c: string;
  h: string;
  l: string;
  v: string;
  n: number;
}

export interface Position {
  coin: string;
  szi: string;
  entryPx: string | null;
  positionValue: string;
  unrealizedPnl: string;
  returnOnEquity: string;
  liquidationPx: string | null;
  marginUsed: string;
  maxLeverage: number;
  leverage: { type: "cross" | "isolated"; value: number; rawUsd?: string };
  cumFunding?: { allTime: string; sinceOpen: string; sinceChange: string };
}

export interface MarginSummary {
  accountValue: string;
  totalNtlPos: string;
  totalRawUsd: string;
  totalMarginUsed: string;
}

export interface ClearinghouseState {
  marginSummary: MarginSummary;
  crossMarginSummary: MarginSummary;
  crossMaintenanceMarginUsed: string;
  withdrawable: string;
  assetPositions: { type: string; position: Position }[];
  time: number;
}

export interface OpenOrder {
  coin: string;
  side: "B" | "A";
  limitPx: string;
  sz: string;
  oid: number;
  timestamp: number;
  origSz: string;
  triggerCondition: string;
  isTrigger: boolean;
  triggerPx: string;
  reduceOnly: boolean;
  orderType: string;
  tif: string | null;
  cloid: string | null;
}

export interface Fill {
  coin: string;
  px: string;
  sz: string;
  side: "B" | "A";
  time: number;
  startPosition: string;
  dir: string;
  closedPnl: string;
  hash: string;
  oid: number;
  crossed: boolean;
  fee: string;
  tid: number;
  feeToken: string;
}

export interface HistoricalOrder {
  order: OpenOrder;
  status: string;
  statusTimestamp: number;
}

export interface SpotBalance {
  coin: string;
  token: number;
  total: string;
  hold: string;
  entryNtl: string;
}

export interface LeverageSetting {
  type: "cross" | "isolated";
  value: number;
}
