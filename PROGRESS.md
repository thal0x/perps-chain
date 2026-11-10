# PROGRESS

_Read this, DECISIONS.md and `scripts/status.sh --quick` output first after any context reset._

## Completed milestones
- M0 Plan — docs, interfaces crate, status tooling.

## In progress (owner -> directory, worktree, branch)
(see "Agents" below; worktrees under /home/user/wt/<name>, branch agent/<name>)

| agent (worktree /home/user/wt/<name>) | owns | status |
|---|---|---|
| hlwire | crates/hlwire | MERGED (35 tests, 94 SDK golden vectors) |
| api | crates/api | running |
| consensus | crates/consensus | running |
| netmempool | crates/net, crates/mempool | MERGED (32 tests) |
| bookoracle | crates/book, crates/oracle | MERGED (58 tests) |
| clearinghouse | crates/clearinghouse | MERGED; fill 0.6–2.6 µs (target 1 µs) |
| state | crates/state | running |
| bridge | bridge/, crates/relayer | MERGED (41 forge + 26 rust); e2e script pending node |
| sim | crates/sim | running (wave 2) |
| (unassigned) | crates/node, crates/chaos | after state lands |

## Currently failing
- All component crates are stubs (0 tests) — expected until M1.
- No acceptance evidence yet.

## Next three actions
1. Wait for wave-1 agents; review + merge each branch (build + tests green).
2. Merge component branches as they land; keep workspace green.
3. Build `node` wiring + localnet scripts (M2).

## Environment notes
- Foundry: `~/.local/bin/{forge,anvil,cast}` symlinked from `/opt/foundry/node_modules/@foundry-rs/*-linux-amd64/bin` (npm install). Re-create if container is fresh: `cd /opt/foundry && npm i @foundry-rs/forge @foundry-rs/anvil @foundry-rs/cast`.
- Reference clones (not part of repo): `/home/user/ext/{hyperliquid-python-sdk,hyperliquid (nktkas TS SDK),forge-std}`.
- Shared cargo target dir: `/home/user/.cargo-target/hlbft`.
- 4 cores, 15 GB RAM, ~31 GB disk.

## Blockers
- GitHub push to thal0x/perps-chain: session credential lacks access (Claude GitHub App not installed on repo?). User asked to fix at claude.ai/connect-github; retry add_repo(access=push) when told.
