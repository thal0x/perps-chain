# PROGRESS

_Read this, DECISIONS.md and `scripts/status.sh --quick` output first after any context reset._

## Completed milestones
- M0 Plan — docs, interfaces crate, status tooling.
- M1–M8 — all components merged; all unit suites + forge green (STATUS.md, bf87e52).
- AC2, AC3, AC4, AC5, AC6 PASS with full evidence on the D30-frozen tree (fingerprint 3ffba2b27be4:db263aa35d91:6a118c8621b7:af3525ddaf6f). AC1 smoke passes.
- Extra (user request): `frontend/` trading UI (Vite/React, @nktkas/hyperliquid), Playwright e2e passes. Mac portability fixes (D31).

## In progress
Nothing running. All agents finished; worktrees under /home/user/wt are idle.

## Currently failing
- AC1: no full evidence. **ON HOLD by the user** — do NOT start the 12 h run until the user explicitly asks.

## Next actions (when the user asks for AC1)
1. Pre-AC1 cleanup (changes the fingerprint, so a re-freeze D32): gate `crates/chaos` prctl/PDEATHSIG behind `cfg(target_os = "linux")` (macOS build); make clearinghouse/state proptest configs honour `PROPTEST_CASES`.
2. Rerun AC3, AC5, AC6, AC2 (50M), AC4 on the new frozen tree (~5 h).
3. `scripts/acceptance.sh ac1` (12 h), then `scripts/status.sh --md` on a clean clone, commit M9 "Done".
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
