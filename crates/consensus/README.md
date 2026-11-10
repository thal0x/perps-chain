# consensus

Sans-IO pipelined HotStuff with a 2-chain commit rule and timeout
certificates (Jolteon / DiemBFT-v4 style), a crash-safe file block store, a
threaded driver for the node, and a deterministic network simulator.
Spec: ARCHITECTURE.md §3, DECISIONS.md D1/D3/D8/D14.

## Public API

```rust
// sans-IO core
let mut core = consensus::Core::new(CoreConfig::new(chain_id, genesis_time_ms, validators, me), &mut env);
let outputs = core.handle(ConsensusInput::Message { from, msg }, &mut env); // or Tick / Executed{height, root}
// -> Send / Broadcast / SetTimer{at_ms} / Commit(CommittedBlock) / InstallSnapshot(Snapshot)

// storage
let store = consensus::FileStore::open_with(dir, FileStoreConfig { retain_heights, .. })?;   // impl BlockStore
let mem = consensus::MemStore::new();                                                       // tests

// threaded driver (what the node uses)
let handle = consensus::Driver::spawn(cfg, env /* impl ConsensusEnv + Send */, network /* Arc<dyn Network> */,
                                      inbound /* crossbeam Receiver<(Address, ConsensusMsg)> */,
                                      Box::new(|out| executor_tx.send(out)) /* Commit / InstallSnapshot */);
handle.executed(height, root);                       // executor feedback (or handle.sender() + DriverEvent::Executed)
handle.sender().send(DriverEvent::SaveSnapshot(s));  // persist a snapshot (node: every 1000 heights)
handle.status();                                     // Arc<DriverStatus>: committed_height, view, high_qc_view, divergences
handle.shutdown();

// helpers
consensus::genesis_block(chain_id, genesis_time_ms)  // deterministic height-0 block; all nodes must agree
consensus::crypto::{vote_digest, timeout_digest, payload_hash, secp::{sign, verify, address_of_secret}}
consensus::CertCtx { chain_id, genesis_id, validators }.verify_committed_run(&blocks, anchor, &verify) // checker
```

### Env contract (`ConsensusEnv`, implemented by the node)
* `now_ms` wall clock; `payload` reaps the mempool; `exec_state`/`root_at`
  from the executor (`root_at(0)` = genesis state root); `snapshot` = latest
  local snapshot (e.g. `store.latest_snapshot()`); `sign`/`verify` = secp256k1
  over the 32-byte digest (`crypto::secp` has ready-made helpers); `store`.
* `proposal_override(h)` (replay mode, D8) is called by the **leader and by
  voters**: voters refuse proposals whose `(timestamp, payload hash)` differ
  from the override, and skip the `now + 2 s` drift check for such heights.
  The 2-chain rule needs two successor blocks to commit a block, so to commit
  the last logged height the override must keep returning (e.g. empty, with
  increasing timestamps) blocks for 2 more heights; the node may stop
  executing after the log's end. Pacing in replay mode is
  `local receipt time of parent + min_block_interval`.
* The core never sends to itself and ignores messages whose `from` is itself.

### Executor coupling
`Commit` outputs are strictly height ordered (also across `InstallSnapshot`,
which always precedes the commits that follow it). On restart the core
compares `env.exec_state().0` with the store's committed height and re-emits
the missing commits from the store (in batches of 32 per `handle`, new commits
are held back until the replay finishes); if those heights were pruned it
emits `InstallSnapshot(store.latest_snapshot())` first.

## Protocol notes
* **Views/leaders**: `leader(v) = validators[v mod n]`; stake-weighted quorum
  (`ValidatorSet::quorum_stake`).
* **Votes are broadcast** to all validators (n ≤ 7), so every validator forms
  QC(v) and enters v+1 itself; a crashed next leader costs one view timeout
  and the certified block survives through the timeout's high QC.
* **Voting rule**: `view == current`, `view > max(last_voted, last_timeout)`,
  proposer is `leader(view)` and sent it directly; `justify.view + 1 == view`,
  or TC(view-1) (attached or known) with `justify.view >= tc.high_qc.view`;
  parent header known, height/view/timestamp monotone, timestamp ≤ now+2 s
  (unless replay override), payload hash matches, and `env.root_at(exec_height)`
  must equal `exec_root` — otherwise `tracing::error!("STATE DIVERGENCE ...")`
  and no vote. `SafetyState` is saved through `store.save_safety` before any
  vote/timeout leaves the core (also asserted by the simulator).
* **Vote kinds / domain tags**: a vote's digest is
  `keccak256(tag || borsh(VoteData))` with tag `hlbft/consensus/vote/direct/v1`
  when `justify.view + 1 == view`, else `hlbft/consensus/vote/tc/v1`;
  timeouts use `hlbft/consensus/timeout/v1`. This makes commit proofs
  verifiable from QCs alone (a *direct* QC at `v+1` + a QC at `v` with
  consecutive heights proves the 2-chain commit, since at most one block per
  view is certified).
* **Commit proofs** (`CommittedBlock.commit_proof`): `[QC(B), QC(B')]` for the
  block whose 2-chain fired (B' direct child, view+1); for ancestors committed
  with it: `[QC(A), QC(B), QC(B')]`. `verify_committed_run` accepts a run
  whose blocks link by parent id and whose highest proven block carries a
  direct proof. Sync responders extend a batch (≤ 64 extra) so it ends with a
  direct proof.
* **Pacemaker**: timeout `base(1000) × 1.5^k`, cap 10 s, `k` = consecutive
  views ended by TC, reset when a view ends by QC. After timing out, the
  timeout is re-broadcast every 1 s; f+1 timeouts for the current view make a
  node join (Bracha). Nodes that see a peer behind (lower `Status.high_qc_view`
  or a timeout for an old view) relay their latest block + highest TC
  (rate-limited, certificate-only processing at the receiver).
* **Floating blocks**: a node that is far behind (syncing) can still vote on
  fresh proposals whose parent *header* it has fetched; voting safety depends
  only on QC/TC views. Without this a lagging node that is needed for quorum
  deadlocks with snapshot certification (found by the seed sweep).
* **State sync**: `Status` every 1 s; sync when a peer is > 3 heights ahead or
  our commits stalled for 2 s (peers rotated). Batches ≤ 256 blocks /
  60k txs. Empty response while the peer is ahead ⇒ `SnapshotRequest`; a
  snapshot `(h, root)` is installed only after a verified committed run from
  `h+1` contains a header with `exec_height == h && exec_root == root`.
* **Block cadence**: leader proposes when it holds QC(v-1) or TC(v-1) and
  `now ≥ parent.timestamp + min_block_interval` (default 200 ms); empty blocks
  are proposed as heartbeats.
* **Divergence**: also detected after the fact — a committed header's
  `(exec_height, exec_root)` is compared with `root_at`, and `Executed` roots
  with committed claims. `CoreStats.divergences` counts them.

## FileStore layout (`dir/`)
`blocks.<seq>.log` and `committed.<seq>.log` (segmented append-only logs,
records `len u32 | blake3[..8] | borsh`), `safety.bin` (checksummed, tmp +
fsync + rename + dir fsync), `snapshots/<h>.snap` (same, last 2 kept).
Torn/corrupt tails are truncated on open; committed records reference blocks
by id (block appended and fsynced first). Retention (`retain_heights`,
default 100 000) deletes whole old segments.

## Tests
```
cargo test -p consensus                    # ~1 min: unit, scenarios, proptests, store, driver
cargo test -p consensus --release --test sim_prop long_seed_sweep -- --ignored --nocapture
    # env: HLBFT_SIM_SEEDS (default 200) seeds × n∈{4,7}, HLBFT_SIM_MS (default 300000) virtual ms each
HLBFT_SEED=123 HLBFT_N=7 RUST_LOG=info cargo test -p consensus --test sim_prop debug_one_seed -- --ignored --nocapture
cargo test -p consensus --release --test perf -- --ignored --nocapture   # 20k-tx block cost
```
* `tests/sim_faults.rs` — crashes, 2|2, 3|4, 2|2|3 splits, isolation, lossy +
  duplicating + reordering links, equivocating / double-voting / stale-replay
  / lying Byzantine nodes, FileStore crash-restart, block sync, snapshot sync,
  certified exec roots, diverging executor, replay override, unequal stakes,
  20k-tx blocks.
* `tests/sim_prop.rs` — seeded random fault schedules with safety checker and
  "commits resume ≤ 10 s after heal" liveness check.
* `tests/certs.rs` — QC/TC/commit-proof rules, forged/tampered sync
  responses, uncertified snapshot rejection.
* `tests/store.rs` — reopen, torn tails, kill-at-any-byte proptest,
  retention, corrupt safety file.
* `tests/driver.rs` — 4 threaded drivers, real secp256k1, FileStore restart.

The simulator (`consensus::sim`) is public: `Sim::new(SimConfig::new(n, seed))`,
`run_for`, `set_partition`, `heal`, `crash`, `restart`, `set_byzantine`,
`set_diverging_executor`, `check_safety`.

Perf (release, includes simulator payload generation and cloning): ~20 ms
of CPU per 20k-tx block per node (payload hashed once with a buffered
encoder; blocks held as `Arc<Block>` internally; one clone for the `Commit`
output and one for the proposal broadcast).
