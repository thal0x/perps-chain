# PROGRESS

_Read this, DECISIONS.md and `scripts/status.sh --quick` output first after any context reset._

## Completed milestones
- M0 Plan — docs, interfaces crate, status tooling.

## In progress (owner -> directory, worktree /home/user/wt/<name>, branch agent/<name>)
All wave-1/2 components MERGED: interfaces, hlwire, consensus, net, mempool, book, oracle, clearinghouse, state, api, relayer+bridge, sim, node, chaos, acceptance runners (AC5/AC6).

| agent | owns | status |
|---|---|---|
| spot | crates/state, crates/api (spot PURR/USDC + outcomeMeta, D25) | running |
| integ | crates/node, scripts/localnet.sh, scripts/acceptance.sh, acceptance/ (replay, AC2/3/4 runners, AC3 profiling) | running |

## Currently failing
- AC5: basic_ws `trades PURR/USDC` never gets messages (no spot book -> spot agent); TS market-order guide fails on missing `outcomeMeta` (spot agent); TS withdraw3 "1" rejected (allowlist per D25, integ agent).
- AC1–AC4: runners/evidence pending (integ agent; AC1 12 h run scheduled last on final code).
- AC6: PASS evidence exists but is STALE after later merges (rerun at end).

## Next three actions
1. Merge spot + integ; extend `scripts/localnet.sh seed` with a PURR/USDC ladder.
2. Act on AC3 profiling findings (assign optimizations per crate).
3. Freeze code, run all ACs (AC1 12 h last), commit evidence, README.

## Known risks
- Throughput with one validator down ~2.5 blocks/s (no leader reputation).
- Clearinghouse fill cost 1.6–2.6 µs at 100k accounts.
- sim gen 30–50k actions/s (50M week ≈ 20–30 min) — acceptable.
- AC3 untested at target; 4 validators + load on 4 cores.
- Insurance fund can go negative in a corner case (D19).

## Environment notes
- Foundry: `~/.local/bin/{forge,anvil,cast}` symlinked from `/opt/foundry/node_modules/@foundry-rs/*-linux-amd64/bin` (npm install). Re-create if container is fresh: `cd /opt/foundry && npm i @foundry-rs/forge @foundry-rs/anvil @foundry-rs/cast`.
- Reference clones (not part of repo): `/home/user/ext/{hyperliquid-python-sdk,hyperliquid (nktkas TS SDK),forge-std}`.
- Shared cargo target dir: `/home/user/.cargo-target/hlbft`.
- 4 cores, 15 GB RAM, ~31 GB disk.

## Blockers
- (resolved) Remote `origin` = github.com/thal0x/perps-chain; push `main` after every merge/milestone.
- Disk: ~9 GB free after api merge (D21 profile rebuilds). Prune stale debug/deps once all worktrees are on the D21 profile.
