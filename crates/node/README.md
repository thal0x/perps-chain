# node — the `hlnode` binary

Wires every component into a validator process (ARCHITECTURE.md §2).
CLI and on-disk contract: [docs/NODE_CLI.md](../../docs/NODE_CLI.md).

```
hlnode testnet-gen --validators 4 --out run/localnet [--base-p2p 26600] [--base-api 3001]
                   [--accounts N] [--fund-sdk-examples] [--seed 42] [--genesis-time-ms T]
                   [--bridge-contract 0x..] [--host 127.0.0.1] [--bench]
hlnode run --home run/localnet/node0 [--genesis P] [--config P] [--p2p-listen H:P] [--api-listen H:P]
           [--key P] [--min-block-ms 200] [--invariants off|every|<N>]
           [--oracle-feed synthetic:<seed>[:skew_bps=<i>]|fixed|file:<path>|none]
           [--bridge-rpc http://127.0.0.1:8545] [--replay-log P] [--log-level info]
scripts/localnet.sh start N [--bench] [--release] [--fund-sdk-examples] [-- <run flags>]
scripts/localnet.sh dev | seed | status | stop
```

## testnet-gen
* Keys: `keccak256("hlbft-testnet-key" || seed || role || index || ctr)` (validators
  role `validator`, funded accounts role `account`), equal stake 100.
* SDK example account: fixed, seed independent (`derive_wallet(0, "sdk-example", 0)`,
  address `0x4B88977DA0F07D0454746eFF401bf4f733C2E482`), name `sdk_example` in
  `accounts.json`; funded with 1,000,000 perp USDC, 100,000 spot USDC and 100,000 PURR.
  Funded test accounts get 1,000,000 perp USDC each.
* Markets: BTC (5, 50x), ETH (4, 50x), SOL (2, 20x), AVAX (2, 20x), DOGE (0, 10x),
  ARB (1, 10x). Spot tokens USDC (index 0, tokenId `0x6d1e…e054`) and PURR (index 1,
  tokenId `0xc4bf3f870c0e9465323c0b6ed28096c2`).
* Bridge: `eth_chain_id` 31337, contract `--bridge-contract` or the fresh-Anvil default
  `0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512`, min deposit 5 USDC (Deploy.s.sol default).
* `genesis_time_ms` defaults to now (the synthetic oracle walk is anchored there).
* `config.toml` per node: genesis/key paths (relative to home), listen addresses, oracle
  feed `synthetic:<seed>`, optional `invariants`, `min_block_ms`, `oracle_interval_ms`,
  `bridge_rpc`, `bridge_confirmations`, `snapshot_interval` (default 1000), and a
  `[mempool]` table (`mempool::PoolConfig`). `--bench` writes `invariants = "off"` and
  disables the per-IP / per-signer rate limits.

## run: threads
| thread | role |
|---|---|
| `net` (own tokio rt) | `net::P2p` mesh |
| `cons-inbound` | `NetMsg::Consensus` -> driver channel; records proposal payload hashes (in-flight set) |
| `peer-txs-{0,1}` | `Pool::handle_peer_txs` (checker = `check_tx`) |
| `consensus` | `consensus::Driver` with `NodeEnv` (FileStore under `data/`, secp sign/verify, mempool payload, exec roots, replay override); sink writes `commits.log` and forwards commits |
| `executor` | commits in height order: `execute_block` under the chain write lock, `Executed{h,root}` -> driver, snapshot every 1000 heights (`DriverEvent::SaveSnapshot`), `roots.log`, `remove_committed`, BlockOutcome -> API subscribers |
| `oracle` | every 1 s signs `ValidatorAction::OraclePrices` from the PriceFeed |
| `api` (tokio, 2 workers) | `api::Shared` + `serve_listener` on `--api-listen`; `relayer::DepositWatcher` if `--bridge-rpc` |

* State is `RwLock<HlChain>`; queries/`check_tx` take the read lock. Signature recovery for
  API and peer txs runs *outside* the lock (`Shared::precheck` fills the chain's `SigCache`).
* The consensus env never touches the chain lock: exec height/roots live in `ExecInfo`.
* Validator txs (oracle, bridge) use nonce `max(last+1, now_ms)` allocated under a mutex
  together with mempool insertion, so they enter the pool in nonce order (and stay
  increasing across restarts).
* In-flight filter (`inflight.rs`): the core doesn't exclude txs of uncommitted ancestors,
  so payload() skips txs seen in proposals not yet committed (best effort; duplicates are
  harmless — rejected by nonce).
* Replay (`--replay-log`, D8): `replay.rs` streams `sim::log::LogReader` (window of
  64 records, reads ahead on demand, rewinds by reopening for older heights). The
  **genesis comes from the log header**; a supplied genesis (`--genesis`, or the default
  `<home>/../genesis.json` if it exists) must equal it or the node refuses to start.
  The override returns two empty padding blocks after the log end (+1 ms each); heights
  after the end are not executed. Invariants default to `every` in replay mode and are
  run by the executor with `sim replay` semantics (`check_invariants` + no negative
  equity unless `BadDebtCovered` in the block); the chain's internal checks are off to
  avoid doing the work twice. Oracle feeder and bridge watcher are disabled. Back-pressure:
  `proposal_override` waits (≤ 400 ms) while the local executor is > 8 heights behind, so
  consensus never queues the whole log in memory. A corrupt/truncated log exits the
  process (code 4). When the last logged height executes, `<home>/replay.json` is written:
  `{last_height, final_root, blocks_executed, invariant_checks, violations,
  first_violations, order_actions, roots_digest (= sim replay's), divergences, wall_ms}`.

## Crash safety
* `data/` = consensus `FileStore` (blocks, committed, fsynced safety state, snapshots).
* Restart: restore `latest_snapshot()` (root checked), the core re-emits committed
  blocks above it from the store; the executor re-executes them and compares each root
  with the existing `roots.log` line (`STATE DIVERGENCE` on mismatch, never appended twice).
  `commits.log` likewise. Torn last lines (kill -9 mid-write) are truncated on open.
  Then consensus state sync catches up; double voting is prevented by the persisted
  `SafetyState`.
* SIGTERM/SIGINT: driver, p2p, API stopped; exit 0.

## localnet.sh
`start N` builds (`--release` with `--bench`), runs `testnet-gen` into `run/localnet`
(`LOCALNET_DIR`), launches N nodes (`nodeI/stdout.log`, `node.log`, `hlnode.pid`) and
waits until all report committed height > 2. `dev` = `start 4 --fund-sdk-examples`
(API 3001..3004) + `seed`: rests a 5-level ladder on every market from funded test
account #0 (needs the SDK venv at `/home/user/.venvs/hl`) and writes
`run/localnet/sdk_config.json` (checksummed example account, usable as the SDK
examples' `config.json`).

## Tests
```
cargo test -p node     # unit + tests/net.rs + tests/replay.rs (real processes, random ports), ~30 s
```
* `replay_log_four_validators_twice_matches_offline`: `sim gen` 60-block log (4k actions,
  real HlChain), 4 validators replay it twice (half with `--genesis`, half from the log
  header): all roots.log equal, == `sim replay` roots, run A == run B, invariants checked
  every block, 0 violations, `replay.json` roots_digest == `sim replay`'s.
* `replay_log_refuses_mismatching_genesis`.
* `four_validators_orders_roots_and_crash_recovery`: 4 validators; signed `/exchange`
  order rests and shows in `openOrders` on all nodes; kill -9 node3, more blocks + an
  order, restart, catches up, sees both orders; `roots.log`/`commits.log` agree across
  nodes; no divergence; contiguous roots.log after restart.
* `single_validator_m2`: one validator, order + clearinghouseState, snapshot every 5
  heights, SIGTERM => exit 0, restart from snapshot keeps the order, kill -9 restart.
