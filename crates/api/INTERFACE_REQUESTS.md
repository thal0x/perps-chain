# Interface requests (api)

1. ~~`StateQuery::leverage_of`~~ — done in rev 4 (D22); used by activeAssetData for flat accounts.
2. ~~Block timestamp~~ — resolved by using `StateQuery::block_time()` for all response/push `time` fields.
3. **`RestingOrderView`: `is_position_tpsl: bool` and `children: Vec<Oid>`**
   (or the parent oid). These would fill `frontendOpenOrders.isPositionTpsl`
   and `children`; they are currently always `false` and `[]`.
4. Optional: **`Event::Ledger` for both sides of transfers**. The api adds the
   record to the receiving user itself, unless the state already emitted one
   with the same tx hash. A doc line in `exec.rs` saying which side is emitted
   would remove the guesswork.
5. **Spot hold / spot ctx (D25).** spotClearinghouseState `hold` is
   recomputed from `open_orders` with `api::spot::order_hold` (same formula as
   `state::spot::order_hold`, cross-checked in `tests/spot_chain.rs`), and
   spot volume comes from indexed fills. A `StateQuery::spot_hold(user)` (see
   `crates/state/INTERFACE_REQUESTS.md` 8) would remove the duplication.
   Note: `market_ctx` must only be called for perps and the tradable spot
   asset 10000 (the clearinghouse indexes markets by asset); the API guards
   this via `Coins::tradable_spot_asset`.
