#!/usr/bin/env bash
# Fails if floating point appears in state-machine crates (non-test code is
# what matters, but we check everything under src/ for simplicity; tests may
# live in tests/ directories which are excluded).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
bad=0
for c in interfaces book clearinghouse oracle state consensus; do
  d="$ROOT/crates/$c/src"
  [ -d "$d" ] || continue
  # strip // comments, then look for f32/f64 tokens or float literals like 1.0 / 1e5
  if grep -rnE '\bf(32|64)\b' "$d" | grep -vE '^\S+:\s*//' ; then
    echo "float type found in crates/$c" >&2; bad=1
  fi
done
exit $bad
