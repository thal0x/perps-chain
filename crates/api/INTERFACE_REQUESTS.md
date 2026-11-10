# Interface requests (api)

1. **`StateQuery::leverage_of(&self, user: &Address, asset: AssetId) -> Leverage`**
   `activeAssetData` and `webData2` need the user's leverage setting for
   assets without a position. After `updateLeverage` on a flat account this
   is the only way to show it. The api currently shows the position's
   leverage, or else the HL default of 20x cross, capped at maxLeverage and
   isolated for only-isolated markets.
   ```rust
   fn leverage_of(&self, user: &Address, asset: AssetId) -> Leverage;
   ```
2. **Block timestamp in `BlockOutcome`** (`pub timestamp_ms: TimestampMs`).
   Without it, candles and history use event timestamps (fine), but WS push
   `time` fields and `clearinghouseState.time` use the API's wall clock.
3. **`RestingOrderView`: `is_position_tpsl: bool` and `children: Vec<Oid>`**
   (or the parent oid). These would fill `frontendOpenOrders.isPositionTpsl`
   and `children`; they are currently always `false` and `[]`.
4. Optional: **`Event::Ledger` for both sides of transfers**. The api adds the
   record to the receiving user itself, unless the state already emitted one
   with the same tx hash. A doc line in `exec.rs` saying which side is emitted
   would remove the guesswork.
