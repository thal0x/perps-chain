# Milestones

Each milestone ends with a commit whose message starts with the milestone
name, PROGRESS.md and STATUS.md updated.

| # | Name | Exit check (verifiable) |
|---|------|-------------------------|
| M0 | Plan | ARCHITECTURE/TEST_PLAN/MILESTONES/DECISIONS/PROGRESS committed; `cargo build --workspace` ok; `cargo test -p interfaces` passes; `scripts/status.sh` runs |
| M1 | Components | Each component crate has its unit + property suites passing: `cargo test --workspace` green; `forge test` green incl. replay/threshold/rotation tests; hlwire signature vectors from both SDKs verify |
| M2 | Single node | `hlnode` with 1 validator: Python SDK `basic_order.py` places & cancels against it; state root deterministic across two restarts from the block log |
| M3 | Network | 4- and 7-validator localnet over TCP with mempool gossip; kill/restart a node -> it state-syncs and resumes; chaos smoke (3 min) shows 0 violations |
| M4 | SDK | AC5 full suite passes (Python + TS) |
| M5 | Bridge | AC6 e2e passes |
| M6 | Determinism + invariants | AC2 smoke and AC4 proptests pass; then full 50M-action week replay evidence (AC2, AC4) |
| M7 | Throughput | AC3 evidence ≥ 100k actions/s, median block < 1 s |
| M8 | Chaos 12h | AC1 evidence: 12 h, 7 validators, 0 violations, all resumes ≤ 10 s |
| M9 | Done | Clean clone: `scripts/status.sh` shows all ACs PASS; README complete |
