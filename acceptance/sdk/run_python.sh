#!/usr/bin/env bash
# AC5 (Python): run the official hyperliquid-python-sdk examples, verbatim, against
# a running, freshly seeded localnet (scripts/localnet.sh dev; acceptance.sh ac5
# sets that up).
#
#   acceptance/sdk/run_python.sh [OUT_JSON]
#
# Steps (TEST_PLAN AC5, D6, D23):
#  1. copy the in-scope examples + example_utils.py from the pinned SDK clone into
#     a temp dir, replace only `constants.TESTNET_API_URL` with
#     "http://127.0.0.1:3001" (sed), print the diff and assert it changes nothing else;
#  2. config.json := run/localnet/sdk_config.json (checksummed example account);
#  3. run each example in ORDER (fixed; see below) with a timeout. The observation
#     hook py_trace/sitecustomize.py (PYTHONPATH) records every SDK HTTP call and WS
#     message without changing behaviour;
#  4. judge each with judge_python.py (exit code, errors vs expected_errors.json,
#     WS channel receipt) and write OUT_JSON (default acceptance/tmp/python.json).
#
# Env: HL_SDK_DIR (default /home/user/ext/hyperliquid-python-sdk),
#      HL_PYTHON (default /home/user/.venvs/hl/bin/python; created if missing),
#      LOCALNET_DIR (default <repo>/run/localnet), WS_SECS (default 40),
#      SCRIPT_TIMEOUT (default 120), ONLY="a b" (debug: run a subset; not for evidence).
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
SDK="${HL_SDK_DIR:-/home/user/ext/hyperliquid-python-sdk}"
PY="${HL_PYTHON:-/home/user/.venvs/hl/bin/python}"
NET="${LOCALNET_DIR:-$ROOT/run/localnet}"
URL="http://127.0.0.1:3001"
WS_SECS="${WS_SECS:-40}"
SCRIPT_TIMEOUT="${SCRIPT_TIMEOUT:-120}"
OUT="${1:-$ROOT/acceptance/tmp/python.json}"
ALLOW="$HERE/expected_errors.json"

# Fixed run order from a fresh, seeded localnet. Rationale:
# * basic_order / _with_cloid / _modify first (resting bids far below mid; the
#   modify example leaves its modified order resting) -> cancel_open_orders then has
#   a real open order to cancel;
# * basic_leverage_adjustment runs while ETH is flat: its update_isolated_margin
#   is then rejected as on real HL (expected_errors.json, D23);
# * basic_market_order needs resting liquidity -> the `localnet.sh seed` ladder;
# * basic_tpsl opens an ETH short and rests TP/SL children; basic_schedule_cancel
#   then cancels everything that rests (its own order and those children);
# * transfers / withdraw / agent; the two long-running WS examples last, each with
#   the ws_activity.py fixture so user channels receive events.
ORDER=(basic_order basic_order_with_cloid basic_order_modify cancel_open_orders
       basic_leverage_adjustment basic_market_order basic_tpsl basic_schedule_cancel
       basic_agent basic_transfer basic_spot_transfer basic_spot_to_perp basic_withdraw
       basic_adding basic_ws)
WS_SCRIPTS=" basic_adding basic_ws "
[ -n "${ONLY:-}" ] && read -r -a ORDER <<<"$ONLY"

die() { echo "run_python: $*" >&2; exit 2; }

# ---- SDK + venv ----------------------------------------------------------------
[ -d "$SDK/examples" ] || die "SDK clone not found at $SDK"
SDK_REV="$(git -C "$SDK" rev-parse HEAD)"
if [ ! -x "$PY" ]; then
  echo "== creating venv $(dirname "$(dirname "$PY")")"
  python3 -m venv "$(dirname "$(dirname "$PY")")" || die "venv creation failed"
  "$PY" -m pip install -q -e "$SDK" || die "pip install failed"
fi
"$PY" -c "import hyperliquid, os, sys; p=os.path.realpath(hyperliquid.__file__); sys.exit(0 if p.startswith(os.path.realpath('$SDK')) else 1)" \
  || die "venv $PY does not import hyperliquid from $SDK"
[ -f "$NET/sdk_config.json" ] || die "no $NET/sdk_config.json (run scripts/localnet.sh dev first)"
curl -sf -m 3 "$URL/admin/status" >/dev/null || die "no node API at $URL"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/ac5py.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/examples" "$WORK/runs" "$WORK/orig"

# ---- 1. verbatim copy + URL substitution -----------------------------------------
for s in example_utils "${ORDER[@]}"; do
  cp "$SDK/examples/$s.py" "$WORK/orig/$s.py" || die "missing example $s"
  cp "$SDK/examples/$s.py" "$WORK/examples/$s.py"
done
sed -i 's|constants\.TESTNET_API_URL|"http://127.0.0.1:3001"|g' "$WORK/examples/"*.py
echo "== diff (SDK $SDK_REV -> run copy)"
(cd "$WORK" && diff -ru orig examples) || true
# Assert: the only change is TESTNET_API_URL -> the local URL. Reverse the
# substitution on the copy; the result must be byte-identical to the original.
DIFF_OK=1
CHANGED=0
for s in example_utils "${ORDER[@]}"; do
  if ! sed 's|"http://127\.0\.0\.1:3001"|constants.TESTNET_API_URL|g' "$WORK/examples/$s.py" | cmp -s - "$WORK/orig/$s.py"; then
    echo "DIFF CHECK FAILED: $s.py changed beyond the URL" >&2; DIFF_OK=0
  fi
  if grep -q '"http://127.0.0.1:3001"' "$SDK/examples/$s.py"; then
    echo "DIFF CHECK FAILED: $s.py already contained the local URL" >&2; DIFF_OK=0
  fi
  CHANGED=$((CHANGED + $(grep -c 'constants\.TESTNET_API_URL' "$WORK/orig/$s.py")))
done
[ "$DIFF_OK" = 1 ] || die "diff check failed"
echo "== diff check ok: $CHANGED occurrence(s) of constants.TESTNET_API_URL replaced, nothing else changed"

# ---- 2. config.json -----------------------------------------------------------
cp "$NET/sdk_config.json" "$WORK/examples/config.json"

# ---- 3. run -------------------------------------------------------------------
export PYTHONPATH="$HERE/py_trace"
export PYTHONUNBUFFERED=1
RESULTS="$WORK/results.jsonl"; : >"$RESULTS"
for s in "${ORDER[@]}"; do
  R="$WORK/runs"
  export HL_ACC_TRACE="$R/$s.trace"
  : >"$HL_ACC_TRACE"
  t0=$(date +%s.%N)
  if [[ "$WS_SCRIPTS" == *" $s "* ]]; then
    echo "== $s (long-running WS example, ${WS_SECS}s window, fixture ws_activity.py at +10s)"
    (cd "$WORK/examples" && exec "$PY" "$s.py") >"$R/$s.stdout" 2>"$R/$s.stderr" &
    pid=$!
    sleep 10
    frc=0
    (cd "$WORK/examples" && HL_ACC_TRACE="$R/$s.fixture.trace" "$PY" "$HERE/ws_activity.py" config.json "$URL") \
      >"$R/$s.fixture.out" 2>&1 || frc=$?
    sed 's/^/   fixture: /' "$R/$s.fixture.out" | cut -c1-300
    end=$(echo "$t0 + $WS_SECS" | bc)
    while [ "$(echo "$(date +%s.%N) < $end" | bc)" = 1 ] && kill -0 "$pid" 2>/dev/null; do sleep 0.5; done
    alive=false
    if kill -0 "$pid" 2>/dev/null; then alive=true; kill -TERM "$pid" 2>/dev/null; sleep 1; kill -9 "$pid" 2>/dev/null; fi
    wait "$pid" 2>/dev/null; rc=$?
    el=$(echo "$(date +%s.%N) - $t0" | bc)
    printf '{"rc": %d, "elapsed_s": %.1f, "alive_at_stop": %s, "fixture": {"name": "ws_activity.py", "rc": %d}}\n' \
      "$rc" "$el" "$alive" "$frc" >"$R/$s.meta.json"
    v=$("$PY" "$HERE/judge_python.py" --script "$s" --dir "$R" --allowlist "$ALLOW" --ws --min-ws-s 30)
  else
    echo "== $s"
    (cd "$WORK/examples" && timeout "$SCRIPT_TIMEOUT" "$PY" "$s.py") >"$R/$s.stdout" 2>"$R/$s.stderr"
    rc=$?
    el=$(echo "$(date +%s.%N) - $t0" | bc)
    printf '{"rc": %d, "elapsed_s": %.1f}\n' "$rc" "$el" >"$R/$s.meta.json"
    v=$("$PY" "$HERE/judge_python.py" --script "$s" --dir "$R" --allowlist "$ALLOW")
  fi
  cut -c1-400 "$R/$s.stdout" | sed 's/^/   | /' | head -40
  [ -s "$R/$s.stderr" ] && cut -c1-400 "$R/$s.stderr" | sed 's/^/   ! /' | tail -15
  echo "$v" >>"$RESULTS"
  echo "$v" | "$PY" -c 'import json,sys; v=json.load(sys.stdin); print("   =>", "PASS" if v["pass"] else "FAIL", "; ".join(v["reasons"]))'
done

# ---- 4. summary ---------------------------------------------------------------
mkdir -p "$(dirname "$OUT")"
"$PY" - "$RESULTS" "$OUT" "$SDK_REV" "$CHANGED" <<'EOF'
import json, sys
res = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
out = {"sdk": "hyperliquid-python-sdk", "sdk_commit": sys.argv[3], "url": "http://127.0.0.1:3001",
       "url_replacements": int(sys.argv[4]), "diff_check": "ok",
       "passed": sum(r["pass"] for r in res), "failed": sum(not r["pass"] for r in res),
       "order": [r["script"] for r in res], "scripts": res}
json.dump(out, open(sys.argv[2], "w"), indent=1)
print(f"== python examples: {out['passed']} passed, {out['failed']} failed")
for r in res:
    print(f"   {'PASS' if r['pass'] else 'FAIL'}  {r['script']}  {'; '.join(r['reasons'])}")
sys.exit(0 if out["failed"] == 0 else 1)
EOF
