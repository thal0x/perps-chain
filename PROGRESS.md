# PROGRESS

_Read this, DECISIONS.md and `scripts/status.sh --quick` output first after any context reset._

## Completed milestones
- M0 Plan — docs, interfaces crate, status tooling.
- M1–M8 — all components merged; all unit suites + forge green (STATUS.md, bf87e52).
- D33 re-freeze (chaos builds on macOS; proptests honour `PROPTEST_CASES`, AC4 full = 10,000 cases/test). AC2, AC3, AC4, AC5, AC6 PASS with full evidence on that tree (fingerprint b5fa40c37607:db263aa35d91:6a118c8621b7:af3525ddaf6f). AC1 smoke passes. STATUS.md regenerated.
- Extra (user request): `frontend/` trading UI (Vite/React, @nktkas/hyperliquid), Playwright e2e passes. Mac portability fixes (D31).
- Extra (user request): `services/` faucet (:3100), market maker, 4 trader bots; `scripts/localnet.sh dev --bots`.

## In progress
Nothing running. All agents finished; worktrees under /home/user/wt are idle.

## Currently failing
- AC1: full 12 h run waived by the user (D32); status.sh reports it as missing evidence, by design.

## Next actions
1. Only if the user asks: `scripts/acceptance.sh ac1` (12 h), then `scripts/status.sh --md`.
2. Confirm on the user's Mac that `cargo build --release` works without `--exclude chaos`, then drop that README note.
3. Bot caveats: degen liquidation rare (~1–2/h); stale seed ladder can pin mid after oracle jumps; localnet.sh `--bots` untested on bash 3.2.
Optional node API gaps found by the frontend: WS `spotState` and `webData3` subscriptions missing.

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
