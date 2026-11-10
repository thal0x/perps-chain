# Interface requests from `state`

None of these block the state crate (local adapters exist); they would make
the contracts explicit.

1. **Construction / decoding is not part of the engine traits.**
   `OrderBook`, `TriggerStore`, `Clearinghouse` have `encode()` but no
   `decode`/constructor, and `OracleAggregator` has neither. The state crate
   uses its own `state::Engines` bundle trait (associated types + factory /
   decode functions). Proposal (optional): add
   `fn decode(bytes: &[u8]) -> Result<Self, String> where Self: Sized;` to
   `OrderBook`, `TriggerStore`, `Clearinghouse`.

2. **`Clearinghouse::take_dirty` after construction / decode must report every
   entity.** The state root needs the full `(StateKey, hash)` set at genesis
   and after `restore`. Proposal: document on `take_dirty`:
   ```rust
   /// Drain the set of (key, entity hash) changed since the last call. The
   /// first call after construction from genesis or from `encode()` bytes
   /// reports every entity.
   ```

3. **StateKey ownership.** The state crate commits `Global`, `UserMeta(_)`,
   `Book(_)`, `Oracle`, `Bridge`; the clearinghouse is expected to use only
   `Account(_)`, `Market(_)`, `Insurance`. (If both report the same key, both
   hashes are included, state's first — still deterministic.) Proposal:
   state this on `StateKey`.

4. **Oracle state is mirrored, not encoded.** The chain keeps the latest
   `(px, time)` per (asset, validator) itself (hashed under `StateKey::Oracle`)
   and replays it into a fresh `OracleAggregator` on restore. Requires that
   aggregation depends only on the latest submission per (asset, validator)
   — true for `oracle::OracleState`. No change requested.

5. **`ActionResponse` for `modify`.** The state crate returns
   `Ok(ActionResponse::Default)` for a successful single `modify` and
   `Err(text)` when it fails (HL wire shape), and `Order(statuses)` for
   `batchModify`. Proposal: fix the doc comment on `ActionResponse::Order`
   ("also used for batchModify"; modify -> Default).

6. **(Resolved upstream in 3e8c75d; workaround removed, regression test kept.)** Clearinghouse entity-set mismatch.
   `take_dirty` can report the insurance account (accts[0]) as
   `StateKey::Account(INSURANCE)` (e.g. at genesis, where `dirty_list`
   includes index 0), but `all_entity_hashes` (used after `decode`) covers it
   only under `StateKey::Insurance`. A restored node would compute a
   different root. Workaround in `HlEngines::take_dirty`: any
   `Account(INSURANCE)` entry is replaced by the `Insurance` entity
   `hash_borsh(&(account(INSURANCE), ledger()))`, matching
   `all_entity_hashes`. Fix suggestion: never push index 0 in `take_dirty`'s
   account loop (set `dirty_insurance` instead). Covered by
   `tests/determinism.rs::restored_entity_set_equals_live`.

7. **Spot fees into the fee pool (D25).** The clearinghouse fee pool is perp
   cash; the trait has no way to credit it from spot USDC, so spot fees go
   to the spot USDC balance of the reserved account `state::SPOT_FEE_ACCOUNT`
   (`0x…fee5`) via `spot_send` (conservation stays exact: spot USDC is in the
   clearinghouse total). Proposal:
   ```rust
   /// Move `fee` of `user`'s spot USDC (token 0) into the fee pool.
   fn collect_spot_fee(&mut self, user: Address, fee: Usd) -> Result<(), ExecError>;
   /// Pay a maker rebate from the fee pool into spot USDC.
   fn pay_spot_rebate(&mut self, user: Address, amount: Usd) -> Result<(), ExecError>;
   ```
8. **Spot holds in queries (D25).** `AccountView.spot_balances` has no hold;
   the API recomputes it from `open_orders` with the same formula
   (`api::spot::order_hold`, cross-checked against `state::spot::order_hold`
   in `crates/api/tests/spot_chain.rs`). Proposal: `StateQuery::spot_hold(&self,
   user: &Address) -> Vec<(u32, Usd)>` (default empty).
9. **Spot asset ids.** `AssetId` docs say "perp asset index"; spot uses
   10000 + pair index (HL). Proposal: document it on `AssetId` and on
   `Genesis.spot_tokens` (spot trading for pair 0 now exists, D25).
