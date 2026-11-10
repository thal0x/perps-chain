# sim

Seeded load generator and block-log tooling for hlbft (ARCHITECTURE.md §8,
TEST_PLAN.md AC2/AC3/AC4). Library + `sim` binary.

## CLI

```
sim genesis --seed S --validators V --accounts A [--markets BTC,ETH] [--p2p-base-port 9000] -o DIR
    -> DIR/genesis.json, DIR/keys.json (validators, accounts + agent kind, agent wallets)

sim gen --seed S --actions N --sim-days D --block-ms B --validators V --accounts A
        [--invalid-ppm 10000] [--skewed-validators 1] [--app state|null]
        [--roots gen_roots.txt] [--genesis-dir DIR] [--threads T] -o out.log.zst
    runs an embedded state::HlChain (agents get feedback), writes the log,
    prints a JSON summary (footer, final root, roots digest, result histogram,
    fills / liquidations / bad debt, agent stats, actions/s)

sim replay LOG [--check-invariants every|never|N] [--roots out.txt] [--threads T]
    re-executes the log through HlChain; per block: Chain::check_invariants
    (clearinghouse conservation/funding/OI, books uncrossed, bridge) and
    Clearinghouse::negative_equity_accounts (violation unless that account had
    BadDebtCovered in the block). Writes "height root_hex" lines; prints the
    summary incl. roots_digest; exit code 2 on any violation.

sim load --targets http://127.0.0.1:3001,... --rate R --duration S --mode bench|mixed
         [--keys keys.json|accounts.json] [--genesis genesis.json] [--concurrency 4096]
         [--presign-s 3] [--sign-threads 2] [--out report.json]
    R = order actions / s. Pre-signs bodies ahead (producer thread + rayon),
    open-loop paced sender (tokio + reqwest keep-alive). Reports submitted,
    accepted, err_status, http_errors, timeouts, order actions, per-entry
    errors, latency p50/p90/p99/max.

sim info LOG      header + footer
```

AC2: `sim gen --seed 42 --actions 50000000 --sim-days 7 --block-ms 6000 -o week.log.zst`
(100,800 blocks). The node replays with `hlnode run --replay-log week.log.zst`
using the genesis embedded in the log header (`--genesis-dir` writes it as
genesis.json too; it is identical to `sim genesis` with the same params).

## Library

* `genesis::{genesis, genesis_with, default_markets, markets_by_name, SimKeys,
  write_genesis_files}` — deterministic validator/account/agent keys (ChaCha20
  from blake3(seed, domain)); markets BTC ETH SOL AVAX DOGE ARB kPEPE
  (isolated-only); spot USDC(0) + PURR(1); fixed genesis time 2026-01-01.
* `log::{LogWriter, LogReader, LogHeader, BlockRecord, LogFooter, GenParams}` —
  zstd stream: `MAGIC "HLBFTLOG" | u32 ver | u32 len borsh(header)` then
  `0x01 | u32 len | borsh(BlockRecord{height,timestamp_ms,proposer,txs})`
  ... `0xFF | u32 len | borsh(footer{counts, blake3 of all records})`.
  Streaming both ways; reader verifies contiguity, genesis hash and the
  footer digest (truncation is an error). `BlockRecord::context()` gives the
  `BlockContext` (block_id is synthetic; the state root does not depend on it).
* `agents::Population` — the agent population; `step(height, ts, budget,
  &World, &keys) -> Vec<Unsigned>`, `interest()` / `after_block(&World)` for
  feedback. `Mode::Mixed` (gen) / `Mode::Bench` (AC3).
* `sign::{sign_all, sign_one}` — rayon, order-preserving; produces `Tx` and
  optionally the `/exchange` JSON body.
* `app::{SimApp, AppFactory, state_factory, null_factory, World}` — embedded
  application behind `interfaces::Application` (+ invariant hooks);
  `NullApp` has no state (generator benchmarks, tests).
* `gen::generate`, `replay::replay`, `load::run_load`.

## Agents

All decisions come from per-account / scheduler / per-validator ChaCha20
streams and the `World` (built only from `StateQuery`); no clock, no hash
iteration. Account kinds are assigned in contiguous blocks
(`agents::assign_kinds`): market makers 3%, takers 28%, trend 10%, TP/SL 10%,
leverage 8%, transfer 8%, wash 8%, liquidation hunters 3%, degens 22%.

* **Market makers**: two-sided ALO/GTC ladders (5–12 levels/side, cloid on
  every order) around the common reference price (the validators'
  `SyntheticFeed` walk, no noise); requote via batchModify (≤50), modify,
  cancel (by oid when known from `open_orders`, else cancelByCloid) +
  replace, or pull all.
* **Takers**: IOC at 0.3–3% slippage or "market" (5%), heavy-tailed notional
  (Pareto α≈1, integer), reduce-only when over-exposed.
* **Degens**: small balance, max leverage, big IOCs → liquidations.
* **Trend followers**: integer EMA(8)/EMA(64) crossover IOCs; passive GTCs.
* **TP/SL**: `normalTpsl` (entry + TP + SL), `positionTpsl` (sz 0), stops,
  closes.
* **Leverage**: updateLeverage (cross/isolated), updateIsolatedMargin ±,
  scheduleCancel.
* **Transfer**: usdSend, usdClassTransfer, spotSend (PURR), withdraw3,
  approveAgent, then L1 orders signed by the agent wallet.
* **Wash traders**: self-crossing (GTC + crossing IOC in one action → STP),
  and colluding pairs (A rests, partner B takes in the same block).
* **Liquidation hunters**: scan cached account views for equity < 125% of
  maintenance, push the book against the weakest account's largest position
  (sweep IOC + resting order to move the BBO and hence the mark).
* **Validators**: every block each validator submits `OraclePrices` from its
  `SyntheticFeed` (shared seed; honest ones only noise). Up to
  `min(2, (n-1)/3)` skewed validators (+5% fixed; growing negative skew).
  Bridge deposits (ids from 0) attested by honest validators within 0–2
  blocks (occasional duplicates); skewed ones skip or attest conflicting
  amounts. Pending withdrawals (ids from 1) get EIP-712 signatures
  (`relayer::withdrawal_digest`; skewed validators sometimes sign garbage),
  then `WithdrawalFinalized` once a quorum signed.
* **Invalid traffic** (`--invalid-ppm`): reused nonce, nonce outside the
  window, corrupted signature, unknown signer, too-precise price, huge size
  (insufficient margin), below-$10 order, oversized transfers, bad leverage.

`--actions` counts Σ max(1, order actions) over user txs (an order, cancel or
modify inside a batch counts 1; a transfer counts 1). The per-block budget is
`actions·h/blocks − generated`, so actions spread evenly.

## Performance

Pipelined per block: the executor thread runs block h on the embedded chain
and collects the world delta (market summaries, views of accounts that acted /
are on the liquidation watchlist / in a rotating window of 64, their open
orders, pending withdrawals) while the main thread generates block h+1 from
the world after h-1 and signs it with rayon (order kept). Feedback therefore
lags one block — a fixed schedule, so output stays deterministic. Accounts
that acted in h+1 keep their local order view until h+1's own refresh. Log
writing (borsh + zstd level 3, single-threaded) runs on a third thread.
Known-good txs are primed into the chain's `SigCache` (typed action +
signer), so the embedded chain skips ECDSA recovery during generation
(gen roots == replay roots is tested).

Measured on the shared 4-core box (load avg ~4.5 from other agents),
release, `--threads 2`, 2000 accounts, AC2 density (~500 actions / 6 s
block), 1M actions:

| | wall | actions/s |
|---|---|---|
| `gen --app null` (generate + sign + log only) | 11-13 s | 75-90k |
| `gen` (embedded HlChain) | 20-33 s | 30-50k |
| `replay --check-invariants every` (recovers every signature) | 38 s | 26k |

`phase_ms` in the gen summary splits wall time into generate / sign /
execute / refresh / log back-pressure; executing the block in HlChain is the
bottleneck (~16-24 s per 1M actions under load), then signing (~300k txs
per 1M actions: takers send single orders). Log size: ~36 MB per 1M actions
=> **~1.8 GB projected for the 50M-action AC2 week** (100,800 blocks).

`sim load --mode bench` against an in-process signature-verifying mock
sustained 120k order actions/s (2 targets, 2,800 req/s, p50 2.6 ms) with
bodies pre-signed by 2 threads.

## Tests

```
cargo test -p sim
```

* px proptests: rounded prices always `is_valid_px`, sizes `is_valid_sz`
  and ≥ $10.
* genesis determinism, keys/genesis JSON roundtrip.
* log roundtrip (5000 blocks, byte-at-a-time reader), truncation / gaps /
  missing footer rejected.
* byte-identical logs for the same seed (NullApp; real chain with 1 vs 3
  rayon threads), different seed differs.
* population: every non-deliberately-invalid order has valid px/sz, bodies
  parse back to the same tx, typed decode roundtrips, signatures recover to
  the right account / agent / validator; batches ≤ 50; bench mode emits
  full 50-entry batches.
* end-to-end gen + 2× replay on `state::HlChain`: no invariant violations,
  identical roots across replays and equal to gen-time roots.
