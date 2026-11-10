# PROGRESS

_Read this, DECISIONS.md and `scripts/status.sh --quick` output first after any context reset._

## Completed milestones
- M0 Plan — docs, interfaces crate, status tooling.

## In progress (owner -> directory, worktree, branch)
(see "Agents" below; worktrees under /home/user/wt/<name>, branch agent/<name>)

| agent (worktree /home/user/wt/<name>) | owns | status |
|---|---|---|
| hlwire | crates/hlwire | running (wave 1) |
| api | crates/api | running |
| consensus | crates/consensus | running |
| netmempool | crates/net, crates/mempool | running |
| bookoracle | crates/book, crates/oracle | running |
| clearinghouse | crates/clearinghouse | running |
| state | crates/state | running |
| bridge | bridge/, crates/relayer | running |
| (unassigned) | crates/node, crates/sim, crates/chaos | wave 2 after state+hlwire land |

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
