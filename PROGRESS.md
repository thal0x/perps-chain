# PROGRESS

_Read this, DECISIONS.md and `scripts/status.sh --quick` output first after any context reset._

## Completed milestones
- M0 Plan — docs, interfaces crate, status tooling.

## In progress (owner -> directory, worktree, branch)
(see "Agents" below; worktrees under /home/user/wt/<name>, branch agent/<name>)

| agent | owns | status |
|---|---|---|
| — | — | — |

## Currently failing
- All component crates are stubs (0 tests) — expected until M1.
- No acceptance evidence yet.

## Next three actions
1. Commit M0, create worktrees, spawn component agents (wave 1).
2. Merge component branches as they land; keep workspace green.
3. Build `node` wiring + localnet scripts (M2).

## Environment notes
- Foundry: `~/.local/bin/{forge,anvil,cast}` symlinked from `/opt/foundry/node_modules/@foundry-rs/*-linux-amd64/bin` (npm install). Re-create if container is fresh: `cd /opt/foundry && npm i @foundry-rs/forge @foundry-rs/anvil @foundry-rs/cast`.
- Reference clones (not part of repo): `/home/user/ext/{hyperliquid-python-sdk,hyperliquid (nktkas TS SDK),forge-std}`.
- Shared cargo target dir: `/home/user/.cargo-target/hlbft`.
- 4 cores, 15 GB RAM, ~31 GB disk.
