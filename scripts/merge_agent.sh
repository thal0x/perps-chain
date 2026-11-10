#!/usr/bin/env bash
# Merge an agent branch into main only if the workspace builds and all tests
# pass afterwards. Usage: scripts/merge_agent.sh <name> [extra crates to test...]
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
name="$1"; shift || true
br="agent/$name"
[ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || { echo "not on main"; exit 1; }
git diff --quiet && git diff --cached --quiet || { echo "main has uncommitted changes"; exit 1; }
if ! git merge --no-ff --no-commit "$br"; then
  others=$(git diff --name-only --diff-filter=U | grep -v '^Cargo.lock$' || true)
  if [ -n "$others" ]; then
    echo "merge conflict in: $others; aborting"; git merge --abort; exit 1
  fi
  # Cargo.lock only: take main's and let cargo re-resolve the new deps.
  git checkout --ours Cargo.lock && cargo metadata --format-version 1 >/dev/null && git add Cargo.lock
fi
# Never let an agent branch modify lead-owned paths.
owned=$(git diff --cached --name-only | grep -E '^(crates/interfaces/|Cargo.toml$|ARCHITECTURE.md|DECISIONS.md|TEST_PLAN.md|MILESTONES.md|PROGRESS.md|scripts/status.sh)' || true)
if [ -n "$owned" ]; then
  echo "branch touches lead-owned files:"; echo "$owned"; git merge --abort; exit 1
fi
if ! cargo build --workspace --all-targets 2>&1 | tail -5; then :; fi
if ! cargo build --workspace --all-targets >/dev/null 2>&1; then
  echo "BUILD FAILED after merge; aborting"; git merge --abort; exit 1
fi
if ! cargo test --workspace --no-fail-fast -q 2>&1 | tee /tmp/merge_test_$name.log | grep -E "test result|FAILED|panicked" | tail -40; then :; fi
if grep -qE "test result: FAILED|error\[|could not compile" /tmp/merge_test_$name.log; then
  echo "TESTS FAILED after merge; aborting"; git merge --abort; exit 1
fi
if [ -f bridge/foundry.toml ] && git diff --cached --name-only | grep -q '^bridge/'; then
  (cd bridge && forge test >/tmp/merge_forge.log 2>&1) || { echo "FORGE FAILED"; tail -20 /tmp/merge_forge.log; git merge --abort; exit 1; }
fi
git commit -qm "merge $br into main" && echo "MERGED $br -> $(git rev-parse --short HEAD)"
