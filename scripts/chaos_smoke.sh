#!/usr/bin/env bash
# AC1 smoke: build release hlnode + hlchaos, run a 3-minute 7-validator chaos
# run. Exit 0 iff the run passes. Env overrides:
#   DURATION (3m) VALIDATORS (7) MAX_FAULTY (2) SEED (random) LOAD_RATE (5)
#   WORKDIR (target/chaos-smoke) REPORT (<WORKDIR>/smoke-report.json)
#   EPISODE_MIN (10s) EPISODE_MAX (30s) OBSERVE (10s)  EXTRA_ARGS (passed to hlchaos run)
set -euo pipefail
cd "$(dirname "$0")/.."
TARGET_DIR="${CARGO_TARGET_DIR:-$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')}"
DURATION="${DURATION:-3m}"
VALIDATORS="${VALIDATORS:-7}"
MAX_FAULTY="${MAX_FAULTY:-2}"
SEED="${SEED:-$(( RANDOM * 32768 + RANDOM ))}"
LOAD_RATE="${LOAD_RATE:-5}"
WORKDIR="${WORKDIR:-$TARGET_DIR/chaos-smoke}"
REPORT="${REPORT:-$WORKDIR/smoke-report.json}"
EPISODE_MIN="${EPISODE_MIN:-10s}"
EPISODE_MAX="${EPISODE_MAX:-30s}"
OBSERVE="${OBSERVE:-10s}"

echo "== building release hlnode + hlchaos"
cargo build --release -p node -p chaos
HLNODE="$TARGET_DIR/release/hlnode"
HLCHAOS="$TARGET_DIR/release/hlchaos"
[ -x "$HLNODE" ] || { echo "missing $HLNODE"; exit 2; }
SIM_ARGS=()
if [ -x "$TARGET_DIR/release/sim" ] && "$TARGET_DIR/release/sim" load --help >/dev/null 2>&1; then
  SIM_ARGS=(--sim "$TARGET_DIR/release/sim")
fi
mkdir -p "$WORKDIR"
echo "== hlchaos run: $VALIDATORS validators, $DURATION, max-faulty $MAX_FAULTY, seed $SEED"
set +e
"$HLCHAOS" run --validators "$VALIDATORS" --duration "$DURATION" --max-faulty "$MAX_FAULTY" \
  --seed "$SEED" --hlnode "$HLNODE" --workdir "$WORKDIR" --load-rate "$LOAD_RATE" \
  --episode-min "$EPISODE_MIN" --episode-max "$EPISODE_MAX" --observe "$OBSERVE" \
  --report "$REPORT" "${SIM_ARGS[@]}" ${EXTRA_ARGS:-}
rc=$?
set -e
echo "== report: $REPORT (exit $rc)"
exit $rc
