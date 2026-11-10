# HL API compatibility checklist

✓ = implemented and shape-validated against the `@nktkas/hyperliquid` 0.33.3
response types (`tests/schema.rs`, `additionalProperties: false`) and/or
exercised by the real SDKs (`tests/python_sdk.rs`, `tests/ts_sdk.rs`).
"stub" = valid shape with fixed or empty data, because the feature is out of
scope (D6/D10).

## POST /info

| type | status | notes |
|---|---|---|
| meta (`dex` "" / other -> empty) | ✓ | marginTableId = maxLeverage; one single-tier table per distinct maxLeverage; `onlyIsolated`+`marginMode` for isolated-only |
| metaAndAssetCtxs | ✓ | ctx from `StateQuery::market_ctx`. `impactPxs` = best bid/ask (null if a side is empty). `dayBaseVlm` = 24 1h candles |
| spotMeta / spotMetaAndAssetCtxs | ✓ (nominal ctx) | pair 0 = `PURR/USDC`, others `@i`; an `@1` alias pair always exists (basic_ws subscribes to it). Spot ctx prices nominal "1.0" (no spot books, D10) |
| allMids (`dex`) | ✓ | book mid, else mark, else oracle, else genesis px (so `market_open` works on an empty book); perps only |
| l2Book (`nSigFigs` 2–5, `mantissa` 2/5 with 5) | ✓ | aggregation: bids floor, asks ceil, 20 levels/side; unknown coin -> `null`; spot coins -> empty levels |
| clearinghouseState (`dex`) | ✓ | `assetPositions[].position` incl. `cumFunding`, `maxLeverage`, isolated `rawUsd` (= margin used); `time` = latest block time |
| spotClearinghouseState | ✓ | nonzero balances; `hold`/`entryNtl` "0.0" |
| openOrders / frontendOpenOrders | ✓ | most recent first; `orderType` derived (Limit / Stop Market / Take Profit Limit / ...), `triggerCondition` "N/A" or "Price above/below X"; `children` [] |
| orderStatus (by oid, or by cloid string as Python sends) | ✓ | live orders from state (status `open`, or `triggered` if indexed), else indexer history; `{"status":"unknownOid"}` |
| userFills (`aggregateByTime`) | ✓ | 2000 most recent, newest first; `dir`, `startPosition`, `closedPnl`, `crossed`, `hash`, `oid`, `tid`, `fee`, `feeToken` "USDC", `cloid`, `liquidation`, `twapId` null |
| userFillsByTime | ✓ | ascending, `startTime`/`endTime`, 2000 max |
| userFunding | ✓ | `delta.type` "funding", hash = zero (funding has no tx) |
| userNonFundingLedgerUpdates | ✓ | deposit, withdraw, internalTransfer, spotTransfer, accountClassTransfer, liquidation; transfers also shown to the receiver |
| fundingHistory | ✓ | one point per (coin, funding time); premium sampled from state at indexing |
| candleSnapshot (14 intervals) | ✓ | built from fills; 1w buckets start Monday 00:00 UTC; 1M calendar months; 5000 per series |
| historicalOrders | ✓ | latest status per order, newest first, 2000 max |
| userFees | ✓ | rates from genesis `ChainParams`; `dailyUserVlm` for the last 15 active days |
| userRateLimit | ✓ | `cumVlm` = traded notional, `nRequestsUsed` = receipts, cap 10000+cumVlm |
| perpDexs | ✓ | `[null]` (main dex only) |
| referral | stub | `needToCreateCode` |
| maxBuilderFee | stub | `0` |
| recentTrades | ✓ | last 100, newest first, `users` [buyer, seller] |
| activeAssetData | ✓ | leverage from position, else `StateQuery::leverage_of`, else default 20x cross; available = withdrawable × leverage |
| predictedFundings | ✓ | `[[coin, [["HlPerp", {fundingRate, nextFundingTime, fundingIntervalHours}]]]]` |
| webData2 | ✓ | clearinghouse, open orders, meta, ctxs, spot state |
| exchangeStatus | ✓ | |
| userRole | ✓ | `agent` (via `principal_of`) or `user` |
| extraAgents, subAccounts, userVaultEquities, twapHistory, userTwapSliceFills, leadingVaults, delegations | stub | `[]` / `null` |
| **pendingWithdrawals** (custom) | ✓ | exact relayer wire shape (`relayer::api::to_wire`), round-trip tested |
| **nodeStatus** (custom) | ✓ | `NodeStatus` serde JSON (same as `GET /admin/status`) |

Errors: unknown type, missing or invalid fields, or non-JSON -> HTTP 422 with
a text body (as HL does; Python raises `ClientError`, TS raises
`HttpRequestError`).

## POST /exchange

* Body `{action, nonce, signature{r,s,v}, vaultAddress?, expiresAfter?}`,
  parsed by hlwire, then `submit(Tx::User, TxSource::Api{ip})`, then a wait of
  up to 10 s for the receipt.
* `{"status":"ok","response":{"type":"order","data":{"statuses":[...]}}}`.
  The statuses are `{"resting":{"oid","cloid"?}}`,
  `{"filled":{"totalSz","avgPx","oid","cloid"?}}`, `{"error":..}`,
  `"waitingForTrigger"` and `"waitingForFill"`, as the TS type declares.
  batchModify uses the same shape.
* `{"type":"cancel","data":{"statuses":["success" | {"error":..}]}}` for
  cancel and cancelByCloid.
* `{"type":"default"}` for modify, scheduleCancel, updateLeverage,
  updateIsolatedMargin, usdSend, spotSend, usdClassTransfer, withdraw3,
  approveAgent, setReferrer and noop.
* `{"status":"err","response":"..."}` for execution errors, admission errors,
  malformed bodies (always HTTP 200 + JSON) and timeouts.

## WebSocket /ws (and /)

* Greeting text `Websocket connection established.`.
* `{"method":"ping"}` -> `{"channel":"pong"}`.
* `subscribe` / `unsubscribe` -> `{"channel":"subscriptionResponse","data":{"method",...,"subscription":<echo>}}`.
  Errors are `{"channel":"error","data":"Invalid subscription {..}" | "Already subscribed: {..}" | "Already unsubscribed: {..}"}`.
* `post`: `{"method":"post","id":N,"request":{"type":"info"|"action","payload":..}}` -> `{"channel":"post","data":{"id":N,"response":{"type":"info","payload":{"type":<info type>,"data":..}} | {"type":"action","payload":<exchange response>} | {"type":"error","payload":".."}}}`.

| subscription | channel | snapshot | pushed when |
|---|---|---|---|
| allMids (`dex`) | allMids `{mids, dex?}` | ✓ | every block |
| l2Book (`nSigFigs`,`mantissa`) | l2Book | ✓ | `BookChanged` |
| bbo | bbo | ✓ | `BookChanged` |
| trades | trades | recent (if any) | fills of the coin |
| candle | candle | last candle | fills of the coin |
| orderUpdates | orderUpdates `[{order,status,statusTimestamp}]` | – | user's OrderUpdate events |
| userFills (`aggregateByTime`) | userFills `{user,fills,isSnapshot?}` | ✓ `isSnapshot:true` | user's fills |
| userEvents | user `{fills}` / `{funding}` / `{liquidation}` / `{nonUserCancel}` | – | user's events |
| userFundings | userFundings | ✓ | funding |
| userNonFundingLedgerUpdates | userNonFundingLedgerUpdates | ✓ | ledger |
| notification | notification `{notification}` | – | liquidation |
| webData2 | webData2 | ✓ | every block |
| activeAssetCtx (perp / spot `@1`, `PURR/USDC`) | activeAssetCtx / activeSpotAssetCtx | ✓ | every block (perp) |
| activeAssetData (perp) | activeAssetData | ✓ | every block |
| clearinghouseState (`dex`) | clearinghouseState `{dex,user,clearinghouseState}` | ✓ | every block |
| openOrders (`dex`) | openOrders `{dex,user,orders}` | ✓ | user's order updates or fills |

Spot coins (`PURR/USDC`, `@N`) are accepted wherever a coin is. Spot books
and trades are always empty.

## Known gaps

* Spot prices and volumes are nominal. There are no TWAP, vault, sub-account,
  staking or builder features (out of scope, D6).
* `isPositionTpsl` and `children` in frontendOpenOrders are not populated
  (interface request 3).
* WS pushes every block for the "every block" channels. There is no
  time-based throttling.
