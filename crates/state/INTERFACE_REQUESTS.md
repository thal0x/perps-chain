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

6. **Clearinghouse entity-set mismatch (bug report for `crates/clearinghouse`).**
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
