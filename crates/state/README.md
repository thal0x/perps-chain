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
* `tests/determinism.rs` — seeded random multi-account scenario: identical
  roots/receipts/events on replay, snapshot/restore mid-way, proptest with
  invariants every block (incl. liquidations from price shocks).
