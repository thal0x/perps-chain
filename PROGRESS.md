# PROGRESS

_Read this, DECISIONS.md and `scripts/status.sh --quick` output first after any context reset._

## Completed milestones
- M0 Plan — docs, interfaces crate, status tooling.

## In progress (owner -> directory, worktree, branch)
(see "Agents" below; worktrees under /home/user/wt/<name>, branch agent/<name>)

| agent (worktree /home/user/wt/<name>) | owns | status |
|---|---|---|
| hlwire | crates/hlwire | MERGED (35 tests, 94 SDK golden vectors) |
| api | crates/api | MERGED (30 tests; Python+TS SDK vs fake backend; ~17k req/s); follow-up leverage_of running |
| consensus | crates/consensus | MERGED (44 tests; 600-seed sweep ok, worst resume 6.2 s) |
| netmempool | crates/net, crates/mempool | MERGED (32 tests) |
| bookoracle | crates/book, crates/oracle | MERGED (58 tests) |
| clearinghouse | crates/clearinghouse | MERGED; fill 0.6–2.6 µs (target 1 µs) |
| state | crates/state | MERGED (34 tests, 1.9M actions/s single-thread) |
| bridge | bridge/, crates/relayer | MERGED (41 forge + 26 rust); e2e script pending node |
| sim | crates/sim | running (wave 2) |
| node | crates/node, scripts/localnet.sh | MERGED (4-validator net, kill -9 recovery, 13 Python examples ok); --replay-log awaits sim |
| acceptance | scripts/acceptance.sh, acceptance/ | running (AC5, AC6 full runs) |
| chaos | crates/chaos | running (wave 2) |
| clearinghouse (follow-up) | insurance StateKey fix | MERGED |

## Currently failing
- state `invariants_hold_every_block` proptest: seed 4485867090622990286 -> negative equity after liquidation pass at height 98 (AC4 bug). Regression file kept in scratchpad; clearinghouse agent fixing.
- No acceptance evidence yet.

## Next three actions
1. Merge sim, node, chaos (+ state/api rev-4 follow-ups) as they land; then M1 status + commit.
2. M2/M3: single node + 4/7-validator localnet with SDK smoke (node agent).
3. Write scripts/acceptance.sh runners (ac1..ac6) wiring chaos/sim/sdk/bridge e2e.

## Known risks
- Throughput with one validator down ~2.5 blocks/s (no leader reputation).
- Clearinghouse fill cost 1.6–2.6 µs at 100k accounts.
- Insurance fund can go negative in a corner case (D19).

## Environment notes
- Foundry: `~/.local/bin/{forge,anvil,cast}` symlinked from `/opt/foundry/node_modules/@foundry-rs/*-linux-amd64/bin` (npm install). Re-create if container is fresh: `cd /opt/foundry && npm i @foundry-rs/forge @foundry-rs/anvil @foundry-rs/cast`.
- Reference clones (not part of repo): `/home/user/ext/{hyperliquid-python-sdk,hyperliquid (nktkas TS SDK),forge-std}`.
- Shared cargo target dir: `/home/user/.cargo-target/hlbft`.
- 4 cores, 15 GB RAM, ~31 GB disk.

## Blockers
- (resolved) Remote `origin` = github.com/thal0x/perps-chain; push `main` after every merge/milestone.
- Disk: ~9 GB free after api merge (D21 profile rebuilds). Prune stale debug/deps once all worktrees are on the D21 profile.
