# PROGRESS

_Read this, DECISIONS.md and `scripts/status.sh --quick` output first after any context reset._

## Completed milestones
- M0 Plan — docs, interfaces crate, status tooling.

## In progress (owner -> directory, worktree /home/user/wt/<name>, branch agent/<name>)
All wave-1/2 components MERGED: interfaces, hlwire, consensus, net, mempool, book, oracle, clearinghouse, state, api, relayer+bridge, sim, node, chaos, acceptance runners (AC5/AC6).

| agent | owns | status |
|---|---|---|
| spot | crates/state, crates/api (spot PURR/USDC + outcomeMeta, D25) | MERGED |
| api (perf) | Indexer::ingest 2x cheaper, spawn_blocking submit | running |
| hlwire (perf) | allocation-free decode_action | running |
| integ | crates/node, scripts/localnet.sh, scripts/acceptance.sh, acceptance/ | AC3 PASS + AC5 PASS merged; running AC2 full then AC4 |

## Currently failing
- None in unit suites. Evidence: AC3 PASS (fresh). AC5/AC6 PASS but STALE (rerun queued). AC1/AC2/AC4 full runs pending. AC2+AC4 5M dry run passed (0 mismatches, 0 violations).

## Next three actions (CODE FROZEN, D30)
1. integ agent: full AC5, AC6, then AC2 (50M week, ~4.5 h), AC4 — on the frozen tree.
2. integ agent: AC1 12 h chaos run (7 validators) last.
3. Lead: merge evidence, `scripts/status.sh --md` on a clean clone, final commit M9.

## Known risks
- Throughput with one validator down ~2.5 blocks/s (no leader reputation).
- Clearinghouse fill cost 1.6–2.6 µs at 100k accounts.
- sim gen 30–50k actions/s (50M week ≈ 20–30 min) — acceptable.
- AC3: 110.5k/s at 3.09 cores; ceiling ~150k/s.
- Insurance fund can go negative in a corner case (D19).

## Environment notes
- Foundry: `~/.local/bin/{forge,anvil,cast}` symlinked from `/opt/foundry/node_modules/@foundry-rs/*-linux-amd64/bin` (npm install). Re-create if container is fresh: `cd /opt/foundry && npm i @foundry-rs/forge @foundry-rs/anvil @foundry-rs/cast`.
- Reference clones (not part of repo): `/home/user/ext/{hyperliquid-python-sdk,hyperliquid (nktkas TS SDK),forge-std}`.
- Cargo target dirs: set `CARGO_TARGET_DIR` per worktree (`/home/user/.cargo-target/<name>`; main verification uses `.../main`). No repo-level override since D31.
- 4 cores, 15 GB RAM, ~31 GB disk.

## Blockers
- (resolved) Remote `origin` = github.com/thal0x/perps-chain; push `main` after every merge/milestone.
- Disk: ~9 GB free after api merge (D21 profile rebuilds). Prune stale debug/deps once all worktrees are on the D21 profile.
