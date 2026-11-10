#!/usr/bin/env bash
# Acceptance-criteria runner (TEST_PLAN.md AC1..AC6).
#
#   scripts/acceptance.sh acN [--smoke]
#
# Full run: executes the criterion and writes acceptance/results/acN.json (evidence
# judged by scripts/evidence.py; `source_fingerprint` = evidence.py --fingerprint at
# the start of the run, so commit acceptance/sdk + sources BEFORE a full run).
# --smoke: same checks, result written to acceptance/tmp/acN.smoke.json instead
# (status.sh uses it; it never touches committed evidence).
# Exit code 0 iff the criterion passes.
#
# Adding an AC: write run_acN() below (it gets $MODE = full|smoke and $OUT = the
# JSON path to write) and add it to the dispatch table at the bottom.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"

AC="${1:-}"
MODE=full
[ "${2:-}" = "--smoke" ] && MODE=smoke
TMP="$ROOT/acceptance/tmp"
mkdir -p "$TMP" "$ROOT/acceptance/results"
if [ "$MODE" = full ]; then OUT="$ROOT/acceptance/results/$AC.json"; else OUT="$TMP/$AC.smoke.json"; fi

die() { echo "acceptance: $*" >&2; exit 1; }
log() { echo "[acceptance $AC $(date -u +%H:%M:%S)] $*"; }

# ---- shared helpers ---------------------------------------------------------------
FINGERPRINT=""
start_evidence() {
  FINGERPRINT="$(python3 scripts/evidence.py --fingerprint)"
  if [ "$MODE" = full ] && [ -n "$(git status --porcelain -- crates bridge Cargo.lock acceptance/sdk scripts/acceptance.sh acceptance/bridge_e2e.sh 2>/dev/null)" ]; then
    if [ "${ALLOW_DIRTY:-0}" != 1 ]; then
      git status --short -- crates bridge Cargo.lock acceptance/sdk scripts/acceptance.sh acceptance/bridge_e2e.sh >&2
      die "uncommitted changes in fingerprinted sources: commit first (or ALLOW_DIRTY=1, evidence then marked dirty)"
    fi
  fi
}
GIT_DIRTY() { [ -n "$(git status --porcelain -- crates bridge Cargo.lock acceptance/sdk 2>/dev/null)" ] && echo true || echo false; }

BIN="$ROOT/run/bin"
build_release() {
  log "cargo build --release -p node -p relayer"
  cargo build -q --release -p node -p relayer || die "release build failed"
  local tdir
  tdir="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
  # private copies: the shared target dir can be rebuilt by other worktrees mid-run
  mkdir -p "$BIN"
  cp "$tdir/release/hlnode" "$tdir/release/hlrelayer" "$BIN/"
  HLNODE_BIN="$BIN/hlnode"; HLRELAYER_BIN="$BIN/hlrelayer"
  export HLNODE="$HLNODE_BIN"
}

port_busy() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }
require_free_ports() {
  for p in "$@"; do port_busy "$p" && die "port $p is in use (another localnet/anvil?)"; done
  return 0
}

# ---- AC1: consensus safety (hlchaos) ----------------------------------------------
run_ac1() {
  if [ "$MODE" = smoke ] && [ -x scripts/chaos_smoke.sh ]; then exec scripts/chaos_smoke.sh; fi
  if [ "$MODE" = full ] && [ -x scripts/chaos_full.sh ]; then exec scripts/chaos_full.sh; fi
  echo "ac1: not yet available (hlchaos / scripts/chaos_smoke.sh not landed)"; exit 1
}

# ---- AC2..AC4: wired by the lead once the simulator lands ---------------------------
run_ac2() { echo "ac2: not yet available"; exit 1; }
run_ac3() { echo "ac3: not yet available"; exit 1; }
run_ac4() { echo "ac4: not yet available"; exit 1; }

# ---- AC5: SDK compatibility --------------------------------------------------------
run_ac5() {
  start_evidence
  build_release
  require_free_ports 3001 3002 3003 3004 26600 26601 26602 26603
  local net="$ROOT/run/localnet" py_json="$TMP/ac5_python.json" ts_json="$TMP/ac5_ts.json" prc trc
  export LOCALNET_DIR="$net"
  trap 'scripts/localnet.sh stop >/dev/null 2>&1; rm -rf "$net"' EXIT
  rm -f "$py_json" "$ts_json"

  log "fresh localnet (scripts/localnet.sh dev = 4 validators + seed ladder) for Python examples"
  scripts/localnet.sh dev || die "localnet dev failed"
  acceptance/sdk/run_python.sh "$py_json"; prc=$?
  scripts/localnet.sh stop >/dev/null

  log "fresh localnet for TS examples"
  scripts/localnet.sh dev || die "localnet dev failed"
  acceptance/sdk/run_ts.sh "$ts_json"; trc=$?
  scripts/localnet.sh stop >/dev/null
  rm -rf "$net"

  python3 - "$py_json" "$ts_json" "$OUT" "$FINGERPRINT" "$(GIT_DIRTY)" "$(git rev-parse HEAD)" "$MODE" <<'EOF'
import json, os, sys, datetime
pyp, tsp, out, fp, dirty, head, mode = sys.argv[1:]
py = json.load(open(pyp)) if os.path.exists(pyp) else {"passed": 0, "failed": 1, "scripts": [], "error": "python runner produced no result"}
ts = json.load(open(tsp)) if os.path.exists(tsp) else {"passed": 0, "failed": 1, "scripts": [], "error": "ts runner produced no result"}
fails = [f"py:{s['script']}" for s in py["scripts"] if not s["pass"]] + [f"ts:{s['script']}" for s in ts["scripts"] if not s["pass"]]
ok = py["failed"] == 0 and ts["failed"] == 0 and py["passed"] > 0 and ts["passed"] > 0
ev = {
    "ac": "ac5", "mode": mode, "pass": ok,
    "python_passed": py["passed"], "python_failed": py["failed"],
    "ts_passed": ts["passed"], "ts_failed": ts["failed"],
    "source_fingerprint": fp, "git_head": head, "git_dirty": dirty == "true",
    "time_utc": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "summary": f"python {py['passed']}/{py['passed'] + py['failed']}, ts {ts['passed']}/{ts['passed'] + ts['failed']}"
               + (f"; failing: {', '.join(fails)}" if fails else ""),
    "pins": {"python_sdk": "hyperliquid-python-sdk", "python_sdk_commit": py.get("sdk_commit"),
             "ts_sdk": "@nktkas/hyperliquid", "ts_sdk_version": ts.get("sdk_version"),
             "viem_version": ts.get("viem_version"), "ts_snippet_source_commit": ts.get("snippet_source_commit")},
    "python": py, "ts": ts,
}
json.dump(ev, open(out, "w"), indent=1)
print(f"AC5 {'PASS' if ok else 'FAIL'}: {ev['summary']}  -> {out}")
sys.exit(0 if ok else 1)
EOF
}

# ---- AC6: bridge ----------------------------------------------------------------
run_ac6() {
  start_evidence
  build_release
  require_free_ports 8545 3001 3002 3003 3004 26600 26601 26602 26603
  HLNODE="$HLNODE_BIN" HLRELAYER="$HLRELAYER_BIN" SOURCE_FINGERPRINT="$FINGERPRINT" GIT_DIRTY_FLAG="$(GIT_DIRTY)" \
    acceptance/bridge_e2e.sh "$OUT" "$MODE"
}

case "$AC" in
  ac1) run_ac1 ;;
  ac2) run_ac2 ;;
  ac3) run_ac3 ;;
  ac4) run_ac4 ;;
  ac5) run_ac5 ;;
  ac6) run_ac6 ;;
  *) echo "usage: $0 ac1..ac6 [--smoke]" >&2; exit 2 ;;
esac
