# Rules for component agents

You are one of several engineers building `hlbft` (see ARCHITECTURE.md,
TEST_PLAN.md, DECISIONS.md at the repo root). The lead coordinates.

1. **Work only in your git worktree** (`/home/user/wt/<name>`, branch
   `agent/<name>`), and **edit only the directories you own**. Never edit
   `crates/interfaces`, the root `Cargo.toml`, other crates, or root docs.
   You may add dependencies to *your* crate's Cargo.toml (prefer
   `{ workspace = true }` entries that already exist in the root
   `[workspace.dependencies]`; a new crate version string is fine if needed).
2. **Interface changes go through the lead.** If a type/trait in
   `crates/interfaces` is wrong or missing, do not edit it. Write the proposal
   (what, why, exact Rust diff) in `crates/<yours>/INTERFACE_REQUESTS.md`,
   use a local adapter/extension trait in your crate meanwhile, and mention it
   in your final report.
3. **Determinism**: no `f32`/`f64` in consensus, book, clearinghouse, oracle,
   state (run `scripts/check_no_float.sh`). No HashMap/HashSet iteration in
   state paths (BTreeMap or sorted Vec). No wall clock or RNG in the state
   machine. Overflow = panic. Rounding explicit (`interfaces::fixed`).
4. **Tests**: write thorough unit tests and proptests for your slice of
   TEST_PLAN.md. Never weaken/skip tests to pass. `cargo test -p <crate>`
   must pass; `cargo build --workspace` must still build.
5. **Commit early and often** on your branch (`git -C /home/user/wt/<name>
   commit`), descriptive messages, never force-push or rewrite history. The
   lead merges your branch into `main`. To pick up other components that
   have landed, run `git merge main` in your worktree (only main; resolve
   conflicts only in your files; Cargo.lock conflicts: take either side and
   re-run cargo build).
6. Use your own target dir (`export CARGO_TARGET_DIR=/home/user/.cargo-target/<name>`;
   D24/D31); builds may wait on a lock — that is normal. The machine has 4
   cores shared by ~8 agents: use `cargo test -p <crate>` (never
   `--workspace` loops), avoid release builds unless benchmarking.
7. Write a short `crates/<yours>/README.md`: public API, design notes, how
   to run its tests/benches.
8. Final report (your last message) must include: what's implemented,
   what's not, test counts, known issues, interface requests, and the last
   commit hash on your branch.
