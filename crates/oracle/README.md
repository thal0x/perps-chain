# oracle

Stake-weighted oracle aggregation (state machine) and validator price feeds
(off-chain, in the node). Integer-only.

## `OracleState` (implements `interfaces::OracleAggregator`)

* `OracleState::new(freshness_ms)` / `from_genesis(&Genesis)` (uses
  `params.oracle_freshness_ms`).
* `submit(validator, stake, prices, now)`: upsert the latest
  `(px, stake, time)` per (asset, validator); duplicate asset in one call:
  last wins.
* `aggregate(asset, now, quorum_stake)`: lower stake-weighted median of the
  fresh submissions; `None` if fresh stake `< quorum_stake` (or zero).
* Freshness: fresh iff `now.saturating_sub(time) <= freshness_ms`.
* Extras: `submissions`, `fresh_submissions`, `fresh_stake`, `prune(now)`,
  `remove_validator`, `encode`/`decode` (borsh, BTreeMaps in key order,
  `decode` rejects non-canonical bytes), and the free function
  `weighted_median(&mut [(Px, Address, stake)])`.

### Weighted median definition

Sort fresh submissions by `(px, address)`, accumulate stake `C`; result is the
px of the first entry with `2·C >= T` (T = total fresh stake). This is the
*lower* weighted median — always one of the submitted prices; an exact 50/50
split picks the lower price. Zero-stake entries are ignored.

Skew resistance: if honest validators hold `> T/2` of the fresh stake the
result lies within `[min honest, max honest]`. With
`quorum = floor(2·total/3)+1` and adversaries `< 1/3` of total stake this
always holds whenever `aggregate` returns `Some`.

## `feeds` (implements `interfaces::PriceFeed`)

* `SyntheticFeed::new(markets, SynthParams, start_ms, common_seed,
  validator_seed)`: reference price is a seeded random walk over wall-time
  buckets (default 1 s) starting at `MarketSpec.initial_px` at `start_ms`
  (use genesis time): Irwin–Hall-normal returns (`vol_per_bucket`), jumps
  (`jump_prob_ppm`, `jump_size`), mild mean reversion, clamps. Randomness is
  a counter-based SplitMix64 hash of (seed, asset, bucket), so every
  validator with the same `common_seed` computes the same reference whatever
  its polling pattern. Per-validator uniform noise (`noise`, from
  `validator_seed`), optional `Skew::{Fixed, Growing}` bias for adversarial
  validators, and (default) rounding down to valid HL prices.
  `reference_px(asset, now)` exposes the common price.
* `FixedFeed`: constant prices; clones share the table (`set`/`remove`).
* `FilePriceFeed`: re-reads `<asset_id> <px>` lines (with `#` comments) on
  each call; keeps the last good prices on errors.
* `round_down_to_valid_px(px, spec)`.

## Tests

```
cargo test -p oracle
```

`tests/aggregate.rs` — median cases (weights, even split, ties, zero stake),
brute-force median proptest, quorum, freshness boundary, encoding, skew
resistance proptest, order independence. `tests/feeds.rs` — determinism
across validators and call patterns, noise bounds, skew, valid HL prices, a
week-long walk, end-to-end feeds -> oracle with an adversary.
