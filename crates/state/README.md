# state

The replicated state machine of hlbft (ARCHITECTURE.md §5): executes committed
blocks by driving the order books, clearinghouse, oracle and bridge
bookkeeping, and maintains the state root. Integer-only and deterministic.

## Public API

* **`HlChain`** = `Chain<HlEngines, hlwire::HlVerifier>` — the production
  chain; `HlChain::new_hl(genesis, cfg)`. `HlEngines` = `clearinghouse::
  Clearinghouse`, `book::Book`, `book::TriggerBook`, `oracle::OracleState`.
  `TestChain` = real engines + `FakeVerifier`; `HlChainWith<V>`.
* `Chain<E: Engines, V: TxVerifier>` implements `interfaces::Application` and
  `interfaces::StateQuery`.
  * `Chain::new(genesis, Arc<V>, ChainConfig)`
  * `Engines` (src/engines.rs) bundles the engine types with their
    constructors / snapshot decoders (the interface traits have neither):
    `Ch: Clearinghouse`, `Book: OrderBook`, `Trig: TriggerStore`,
    `Oracle: OracleAggregator`. Optional hooks with defaults:
    `genesis_total`, `withdraw` (HlEngines: `withdraw_with_fee`, fee stays in
    the fee pool, `amount − fee` added to `flows.withdrawals`),
    `take_dirty`, `all_entity_hashes` (HlEngines: rebuild after `decode`),
    `post_liquidation_check` (HlEngines: `negative_equity_accounts()` empty,
    AC4 invariant 3; run in invariant-checking blocks).
  * extras: `check_invariants()`, `invariant_violations()`, `sig_cache()`
    (share with whoever calls `check_tx`), `current_root()`, `flows()`,
    `user_meta()`, `oracle_px()/mark_px()`, `clearinghouse()`, `book()`,
    `triggers()`, `bridge_state()`.
* `ChainConfig`: `invariants` (`Never | EveryBlock | EveryN(n)`, default every
  100), `panic_on_invariant_violation`, `root_window` (10 000),
  `sig_cache_capacity`, `bridge: BridgeHooks { withdrawal_digest, recover }`.
  `ChainConfig::strict()` = every block + panic (tests / replay).
* `bridge_withdrawal_digest` — EIP-712 `Withdrawal(uint64 id,address
  destination,uint256 amount)` digest, byte-identical to
  `relayer::withdrawal_digest` (tested).
* `testkit` — fakes for tests of this and other crates: `FakeVerifier`
  (`action_bytes = borsh(Action)`, signer in `sig.r[..20]`, `v == 27`),
  `user_tx`, `validator_tx`, `SimpleClearinghouse` (cross-only cash-flow
  accounting, exact conservation), `SimpleBook`, `SimpleTriggers`,
  `SimpleOracle`, `test_genesis`, order builders.
  `BookEngines` = real `book`/`oracle` + fake clearinghouse;
  `FakeEngines` = all fakes. Aliases `FakeChain`, `AllFakeChain`.
* Spot (D25, `src/spot.rs`): `SPOT_ASSET` (10000), `SPOT_FEE_ACCOUNT`,
  `SpotPairSpec`, `SpotHold`, `order_hold`, `spot_fee`; chain extras
  `spot_pair()`, `spot_book()`, `spot_hold(user)`, `spot_free(user, token)`,
  `spot_last_px()`. `Engines::spot_balance` (default via `account_view`,
  HlEngines: `Clearinghouse::spot_balance`).
* `HL_ERRORS.md` — every error string returned.

## Block execution (`execute_block`)
1. begin: time = max(prev, ctx.timestamp); fire due scheduled cancels
   (`ScheduledCancel` updates); `on_block_funding`.
2. pre-pass (rayon, pure): decode + recover each user tx (taken from the
   shared `SigCache` if `check_tx` already did it), recover validator txs.
3. txs in payload order (one receipt per user tx).
4. end: oracle aggregate (keeps previous px below quorum) → mark =
   median(oracle, best bid, best ask) or oracle if one-sided → `set_prices`
   → trigger activation (asset order, oid order from `take_triggered`) →
   liquidations (per target: cancel its orders `LiquidatedCanceled`, ledger
   event, reduce-only IOC per close bounded by mark ± `liquidation_slippage`,
   then `backstop_and_adl`) → `BookChanged` per touched market → invariants
   → root.

## Semantics notes / choices
* Orders in a batch are validated independently: tick/lot, min $10 notional
  (reduce-only exempt), duplicate cloid, reduce-only capacity (size clamped
  to the position), initial margin (`check_order_margin`). Status mapping:
  rests ⇒ `Resting` (even if partially filled), else filled ⇒ `Filled`
  (IOC remainder canceled), else error.
* Match hook: STP cancels the maker; reduce-only capacity on both sides
  (partial accept); maker margin re-check (`MarginCanceled`); taker margin
  (`StopTaker`, skipped for reduce-only and liquidations); `apply_fill`;
  `FillEvent` with sequential tids; maker `Filled` order update.
* Triggers: stored at placement (no margin / reduce-only check until fire).
  Standalone ⇒ `Resting{oid}` (the SDK examples cancel them by that oid);
  `positionTpsl` ⇒ `WaitingForTrigger`, size = position at fire time (sz 0
  allowed); `normalTpsl`: first order is the parent, children
  `WaitingForFill` while it rests, `WaitingForTrigger` if it filled at once.
  Children are released when the parent fully fills (as taker or maker),
  removed if the parent leaves the book unfilled, released if it was
  partially filled. Triggered orders keep their oid; market triggers run as
  IOC at their limit px, limit triggers as GTC.
* modify = cancel + new order with a **new oid** (cloid inherited if the new
  order has none). The replacement is validated first; a failing modify
  leaves the original untouched. `modify` ⇒ `Default` / `Err(msg)`,
  `batchModify` ⇒ `Order(statuses)`.
* Nonces per *signer* (agents have their own set): 100 highest, new nonce >
  smallest when full, unused, in (time − 2 d, time + 1 d). The nonce is
  consumed for any tx that passes signature/nonce/expiry/existence checks,
  even if the action then fails. User-signed actions must carry
  `time/nonce == tx.nonce`.
* Agents: L1 actions resolve agent → principal; user-signed actions
  (usdSend, spotSend, usdClassTransfer, withdraw3, approveAgent) act as the
  signer. Unnamed (None or "") replaces the previous unnamed agent; named
  agents persist per name. An address can be agent of one principal only.
* "User or API Wallet … does not exist." if the principal has no
  funds/positions/spot and no agent/referrer/schedule metadata.
* withdraw3: user debited `amount` (gross), `PendingWithdrawal.amount =
  amount − withdraw_fee` (net, D18). With HlEngines the fee goes to the fee
  pool and `flows.withdrawals += amount − fee` (D19). Withdrawal ids start
  at 1; deposit ids may start at 0.
* Backstop/ADL fills returned by the clearinghouse get tids from the chain
  counter.
* Validator txs: ignored silently unless signer == `tx.validator` ∈ set and
  nonce strictly increasing. Deposits credit once when identical
  attestations reach quorum stake (one attestation per validator per id;
  duplicates no-ops). WithdrawalSignature verified via
  `BridgeHooks` (digest + ecrecover), stored sorted by validator, deduped.
  WithdrawalFinalized drops the withdrawal once quorum stake attested.
* scheduleCancel must be ≥ now + 5 s; `None` clears.

## Spot trading (D25)
* One pair: index 0 = token 1 / token 0 ("PURR/USDC"), asset **10000**,
  enabled when genesis has spot tokens 0 and 1. Its book is a `E::Book`
  built from a synthetic `MarketSpec` (`SpotPairSpec::market_spec`).
* Admission: limit orders only (GTC/IOC/ALO; triggers and TP/SL groupings
  rejected), HL spot price rule (5 sig figs, ≤ 8 − szDecimals decimals),
  size multiple of 10^-szDecimals (PURR: whole units), $10 minimum,
  reduce-only rejected (`Reduce only order would increase position.`: no
  spot position), duplicate cloid, and free balance ≥ the order's hold
  (`Insufficient spot balance asset=10000`).
* Holds: resting buy holds `n + ceil(n·max(taker, maker, 0))` USDC with
  `n = ceil(px·sz)`; resting sell holds `sz` PURR. Kept per user in
  `SpotCore.holds` (exactly Σ `order_hold` of the user's resting orders).
  spotSend and spot→perp class transfers may only use free balance.
* Fill (price = maker px): base `sz` seller→buyer, `q = floor(px·sz)` USDC
  buyer→seller, taker fee `ceil(q·taker_fee)`, maker fee `ceil(q·maker_fee)`
  (rebate `floor(q·|maker_fee|)` capped at the taker fee), all via
  `Clearinghouse::spot_send`; net fees go to `SPOT_FEE_ACCOUNT`'s spot USDC
  (the perp fee pool has no spot entry point, INTERFACE_REQUESTS 7). The
  hook pre-checks both parties stay `balance ≥ hold` after the fill (taker
  including the remainder it may rest); a failing maker is canceled
  (`MarginCanceled`), a failing taker stops (`Insufficient spot balance`).
  STP cancels the maker (`SelfTradeCanceled`). `FillEvent` has asset 10000,
  zero closed PnL, start positions = base balance before the fill.
* cancel / cancelByCloid / modify / batchModify / scheduleCancel cover spot
  (modify checks funds with the old order's hold released). Liquidations
  do not touch spot orders.
* Queries: `l2_book`, `mid`, `open_orders`, `order_by_*` include the spot
  book; `market_ctx(10000)` = mark (median of last trade, bid, ask; else
  last, else 1.0) + mid; volume fields zero (the API derives them).
* Root: `StateKey::Book(10000)` = blake3(len ‖ book.encode() ‖
  borsh(SpotCore{holds, last_px})). Snapshot carries the same; `restore`
  re-checks holds against the book. Invariants (every checked block):
  spot book not crossed, holds == Σ order holds, every resting order's user
  in the map, `balance ≥ hold` per token; USDC (incl. the fee account) and
  PURR supply are conserved exactly by the clearinghouse check.

## State root (§5.5)
`root = blake3(height_le8 || Σ_sorted(borsh(StateKey) || hash))` over the
union of clearinghouse entities (`take_dirty`) and state-owned entities:
`Global` (time, counters, flows, validator nonces, TP/SL parents), `Oracle`
(submission mirror + oracle/mark px), `Bridge`, `UserMeta(addr)`,
`Book(asset)` (blake3 of book + trigger encodings). Only dirty entities are
re-hashed. Snapshot = zstd(borsh{core, ch.encode, books, triggers});
`restore` decodes, rebuilds all hashes, recomputes and checks the root.

## Tests
```
cargo test -p state
# throughput (release, single rayon thread, fake verifier, real engines):
# 40 MMs requoting 50-order ladders + 50-entry cancels, 40 IOC takers/block
# measured ~1.9M order actions/s (target >= 300k)
cargo test --release -p state --test bench -- --ignored --nocapture
```
* `tests/orders.rs` — placement, fills, validation errors, ALO, STP, maker
  margin cancel, modify/batchModify/cancelByCloid, scheduleCancel.
* `tests/accounts.rs` — nonces, expiry, bad signature, agents, transfers,
  spot, class transfer, leverage, referrer, duplicate cloid, check_tx, cache.
* `tests/validators.rs` — oracle/mark, deposit quorum, withdrawal flow with
  real secp256k1 signatures, digest == relayer.
* `tests/triggers_liq.rs` — SL/TP, normalTpsl, positionTpsl, liquidation via
  book, backstop + bad debt.
* `tests/hl_e2e.rs` — production `HlChain` with real hlwire wallets:
  L1 orders via agent, user-signed approveAgent/usdSend/withdraw3, signed
  validator oracle txs, tampered signature, replay determinism.
* `tests/spot.rs` — spot rest/hold/cancel, validation, fills + fees +
  balances, GTC partial + ALO + held-balance transfers, STP,
  modify/batchModify/cancelByCloid/scheduleCancel, snapshot/restore,
  seeded random flow (deterministic roots/events, restore), proptest
  `spot_conservation` (USDC/PURR totals exact, holds invariant, every block).
* `tests/determinism.rs` — seeded random multi-account scenario: identical
  roots/receipts/events on replay, snapshot/restore mid-way, proptest with
  invariants every block (incl. liquidations from price shocks).
