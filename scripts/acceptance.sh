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
  log "cargo build --release -p node -p relayer -p sim"
  cargo build -q --release -p node -p relayer -p sim || die "release build failed"
  local tdir
  tdir="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
  # private copies: the shared target dir can be rebuilt by other worktrees mid-run
  mkdir -p "$BIN"
  cp "$tdir/release/hlnode" "$tdir/release/hlrelayer" "$tdir/release/sim" "$BIN/"
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

# ---- AC2 / AC4: block-log replay over real consensus -------------------------------
# Work dir: log, genesis/keys, compressed roots, summaries. Node data is deleted
# after each run (only gzipped roots.log + replay.json are kept).
AC2_DIR="${AC2_DIR:-$ROOT/run/ac2_$MODE}"
REPLAY_MIN_BLOCK_MS="${REPLAY_MIN_BLOCK_MS:-1}"
REPLAY_STALL_S="${REPLAY_STALL_S:-600}"
TOOLS="$ROOT/acceptance/replay_tools.py"

free_port_base() {  # free_port_base START SPAN -> first base with SPAN free ports
  local b="$1" span="$2" ok i
  while [ "$b" -lt 64000 ]; do
    ok=1
    for i in $(seq 0 $((span - 1))); do if port_busy $((b + i)); then ok=0; break; fi; done
    [ "$ok" = 1 ] && { echo "$b"; return 0; }
    b=$((b + span + 1))
  done
  die "no free port range"
}

ac2_params() {
  if [ "$MODE" = full ]; then GEN_ACTIONS=50000000; GEN_DAYS=7; else GEN_ACTIONS=200000; GEN_DAYS=1; fi
  GEN_ACCOUNTS="${GEN_ACCOUNTS:-1000}"
}

# gen_log: `sim gen` into $AC2_DIR; reused when params + source fingerprint match.
gen_log() {
  ac2_params
  local key="seed=42 actions=$GEN_ACTIONS days=$GEN_DAYS accounts=$GEN_ACCOUNTS fp=$FINGERPRINT"
  if [ -f "$AC2_DIR/log.zst" ] && [ -f "$AC2_DIR/gen_summary.json" ] && [ "$(cat "$AC2_DIR/gen.key" 2>/dev/null)" = "$key" ]; then
    log "reusing $AC2_DIR/log.zst ($key)"
    return 0
  fi
  rm -rf "$AC2_DIR"; mkdir -p "$AC2_DIR"
  log "sim gen --seed 42 --actions $GEN_ACTIONS --sim-days $GEN_DAYS --block-ms 6000 (accounts $GEN_ACCOUNTS)"
  if ! "$BIN/sim" gen --seed 42 --actions "$GEN_ACTIONS" --sim-days "$GEN_DAYS" --block-ms 6000 --validators 4 \
      --accounts "$GEN_ACCOUNTS" --threads 2 --genesis-dir "$AC2_DIR" --roots "$AC2_DIR/gen_roots.txt" \
      -o "$AC2_DIR/log.zst" >"$AC2_DIR/gen_summary.json" 2>"$AC2_DIR/gen.stderr"; then
    tail -5 "$AC2_DIR/gen.stderr" >&2; die "sim gen failed"
  fi
  gzip -f "$AC2_DIR/gen_roots.txt"
  tail -c 2000 "$AC2_DIR/gen.stderr" >"$AC2_DIR/gen.stderr.tail"; rm -f "$AC2_DIR/gen.stderr"
  echo "$key" >"$AC2_DIR/gen.key"
  log "log $(du -h "$AC2_DIR/log.zst" | cut -f1): $(python3 -c 'import json,sys;f=json.load(open(sys.argv[1]))["footer"];print(f["blocks"],"blocks",f["actions"],"actions")' "$AC2_DIR/gen_summary.json")"
}

# replay_run LABEL: 4 hlnode validators replay $AC2_DIR/log.zst over real consensus.
REPLAY_PIDS=()
replay_kill() {
  local p
  for p in "${REPLAY_PIDS[@]}"; do kill -9 "$p" 2>/dev/null || true; wait "$p" 2>/dev/null || true; done
  REPLAY_PIDS=()
}
replay_run() {
  local label="$1" rdir="$AC2_DIR/net_$1" api i h now t0 last_h=-1 last_change done_n next_log
  rm -rf "$rdir"; mkdir -p "$rdir"
  python3 "$TOOLS" homes "$AC2_DIR" "$rdir" || die "homes"
  local p2ps=() ports=()
  for i in 0 1 2 3; do p2ps+=("$(cat "$rdir/node$i/p2p")"); ports+=("${p2ps[$i]##*:}"); done
  require_free_ports "${ports[@]}"
  api="$(free_port_base 3301 4)"
  log "replay $label: 4 validators (p2p ${p2ps[0]}.., api $api..), min-block-ms $REPLAY_MIN_BLOCK_MS, invariants every block"
  REPLAY_PIDS=()
  for i in 0 1 2 3; do
    "$BIN/hlnode" run --home "$rdir/node$i" --genesis "$AC2_DIR/genesis.json" --replay-log "$AC2_DIR/log.zst" \
      --p2p-listen "${p2ps[$i]}" --api-listen "127.0.0.1:$((api + i))" --min-block-ms "$REPLAY_MIN_BLOCK_MS" \
      --invariants every --log-level warn >"$rdir/node$i/stdout.log" 2>&1 &
    REPLAY_PIDS+=($!)
  done
  t0=$(date +%s); last_change=$t0; next_log=$((t0 + 60))
  while :; do
    sleep 3
    now=$(date +%s); done_n=0
    for i in 0 1 2 3; do
      if [ -f "$rdir/node$i/replay.json" ]; then done_n=$((done_n + 1)); continue; fi
      if ! kill -0 "${REPLAY_PIDS[$i]}" 2>/dev/null; then
        tail -n 20 "$rdir/node$i/stdout.log" >&2; tail -n 20 "$rdir/node$i/node.log" >&2
        replay_kill; die "replay $label: node$i exited"
      fi
    done
    [ "$done_n" = 4 ] && break
    h=$(for i in 0 1 2 3; do x=$(tail -n1 "$rdir/node$i/roots.log" 2>/dev/null | cut -d' ' -f1); echo "${x:-0}"; done | sort -n | head -1)
    h=${h:-0}
    if [ "$h" != "$last_h" ]; then last_h=$h; last_change=$now; fi
    if [ $((now - last_change)) -gt "$REPLAY_STALL_S" ]; then replay_kill; die "replay $label: no progress for ${REPLAY_STALL_S}s at height $h"; fi
    if [ "$now" -ge "$next_log" ]; then log "replay $label: height >= $h after $((now - t0))s"; next_log=$((now + 300)); fi
  done
  replay_kill
  log "replay $label done in $(( $(date +%s) - t0 ))s"
  for i in 0 1 2 3; do
    gzip -c "$rdir/node$i/roots.log" >"$AC2_DIR/roots_${label}_node$i.txt.gz"
    cp "$rdir/node$i/replay.json" "$AC2_DIR/replay_${label}_node$i.json"
    (grep -c "STATE DIVERGENCE" "$rdir/node$i/node.log" || true) >"$AC2_DIR/divergence_${label}_node$i.txt"
  done
  rm -rf "$rdir"
}

offline_replay() {
  if [ -f "$AC2_DIR/offline_summary.json" ] && [ -f "$AC2_DIR/offline_roots.txt.gz" ]; then log "reusing offline sim replay"; return 0; fi
  log "offline: sim replay --check-invariants every"
  local rc=0
  "$BIN/sim" replay "$AC2_DIR/log.zst" --check-invariants every --roots "$AC2_DIR/offline_roots.txt" --threads 2 \
     >"$AC2_DIR/offline_summary.json.tmp" 2>"$AC2_DIR/offline.stderr" || rc=$?
  if [ "$rc" != 0 ] && [ "$rc" != 2 ]; then tail -5 "$AC2_DIR/offline.stderr" >&2; die "sim replay failed ($rc)"; fi
  mv "$AC2_DIR/offline_summary.json.tmp" "$AC2_DIR/offline_summary.json"
  rm -f "$AC2_DIR/offline.stderr"
  gzip -f "$AC2_DIR/offline_roots.txt"
}

run_ac2() {
  start_evidence
  build_release
  trap 'replay_kill' EXIT
  gen_log
  replay_run A
  replay_run B
  offline_replay
  local rc=0
  python3 "$TOOLS" ac2 "$AC2_DIR" "$OUT" "$FINGERPRINT" "$(GIT_DIRTY)" "$(git rev-parse HEAD)" "$MODE" "$GEN_DAYS" || rc=$?
  # The log is kept for AC4 reuse (same fingerprint); AC2_DELETE_LOG=1 removes it.
  [ "${AC2_DELETE_LOG:-0}" = 1 ] && rm -f "$AC2_DIR/log.zst"
  return $rc
}

# AC4: proptests (case counts from the sources, see acceptance/proptest_cases.py)
# + invariants every block of the AC2 log: offline `sim replay --check-invariants every`
# and the node's replay mode. Reuses the AC2 run's results when they were produced
# from the same source fingerprint and mode; otherwise runs gen + one network replay.
run_ac4() {
  start_evidence
  build_release
  trap 'replay_kill' EXIT
  local plog="$TMP/ac4_cargo_test.log" pjson="$TMP/ac4_proptest.json" rc=0
  log "cargo test -p clearinghouse -p state (PROPTEST_CASES=${PROPTEST_CASES:-default})"
  cargo test -p clearinghouse -p state --no-fail-fast >"$plog" 2>&1 || rc=$?
  python3 acceptance/proptest_cases.py "$plog" "$pjson" crates/clearinghouse crates/state || die "proptest count failed"
  [ "$rc" = 0 ] || log "cargo test failed (rc $rc); see $plog"
  local inv="$AC2_DIR/invariants.json"
  if [ -f "$inv" ] && python3 -c 'import json,sys;j=json.load(open(sys.argv[1]));sys.exit(0 if j["source_fingerprint"]==sys.argv[2] and j["mode"]==sys.argv[3] and j.get("complete") else 1)' "$inv" "$FINGERPRINT" "$MODE"; then
    log "reusing AC2 replay invariant results ($inv)"
  else
    gen_log
    replay_run A
    offline_replay
    python3 - "$AC2_DIR" "$FINGERPRINT" "$MODE" <<'PYEOF'
import json, os, sys
d, fp, mode = sys.argv[1:]
gen = json.load(open(os.path.join(d, "gen_summary.json")))
off = json.load(open(os.path.join(d, "offline_summary.json")))
reps = {f"A{i}": json.load(open(os.path.join(d, f"replay_A_node{i}.json"))) for i in range(4)}
blocks = gen["footer"]["blocks"]
json.dump({"mode": mode, "source_fingerprint": fp, "blocks": blocks, "actions": gen["footer"]["actions"],
           "offline_checks": off["invariant_checks"], "offline_violations": off["violations"][:100],
           "node_checks": {k: v["invariant_checks"] for k, v in reps.items()},
           "node_violations": {k: v["violations"] for k, v in reps.items()},
           "node_first_violations": {k: v["first_violations"][:10] for k, v in reps.items() if v["violations"]},
           "complete": all(v["last_height"] == blocks for v in reps.values()) and off["blocks"] == blocks},
          open(os.path.join(d, "invariants.json"), "w"), indent=1)
PYEOF
  fi
  python3 "$TOOLS" ac4 "$OUT" "$FINGERPRINT" "$(GIT_DIRTY)" "$(git rev-parse HEAD)" "$MODE" "$pjson" "$inv"
}

# ---- AC3: throughput -----------------------------------------------------------------
# 4-validator bench localnet (release, `testnet-gen --bench`: invariants off, API rate
# limits off) + `sim load --mode bench` across all 4 APIs; measured from node0's
# commits.log / roots.log (acceptance/ac3_measure.py).
# Env: AC3_RATE (offered order actions/s, default 110000 full / 12000 smoke),
#      AC3_ACCOUNTS (200), AC3_WARMUP (20), AC3_WINDOW (120 full / 20 smoke),
#      AC3_KEEP=1 keeps run/ac3 (node logs) for profiling.
run_ac3() {
  start_evidence
  build_release
  local warm="${AC3_WARMUP:-20}" win rate min_rate
  if [ "$MODE" = full ]; then win="${AC3_WINDOW:-120}"; rate="${AC3_RATE:-110000}"; min_rate=100000
  else win="${AC3_WINDOW:-20}"; rate="${AC3_RATE:-12000}"; min_rate=10000; fi
  local net="$ROOT/run/ac3" api p2p dur
  api="$(free_port_base 3401 4)"; p2p="$(free_port_base 26700 4)"
  export LOCALNET_DIR="$net" BASE_API="$api" BASE_P2P="$p2p" ACCOUNTS="${AC3_ACCOUNTS:-200}" HLNODE="$BIN/hlnode"
  AC3_NET="$net"
  trap 'scripts/localnet.sh stop >/dev/null 2>&1; [ "${AC3_KEEP:-0}" = 1 ] || rm -rf "$AC3_NET"' EXIT
  log "localnet: 4 validators --bench --release (api $api.., p2p $p2p..)"
  scripts/localnet.sh start 4 --bench --release >/dev/null || die "localnet start failed"
  dur=$((warm + win + 15))
  local targets="" i
  for i in 0 1 2 3; do targets="${targets:+$targets,}http://127.0.0.1:$((api + i))"; done
  log "sim load --mode bench --rate $rate --duration $dur across 4 APIs"
  "$BIN/sim" load --targets "$targets" --rate "$rate" --duration "$dur" --mode bench \
      --keys "$net/accounts.json" --genesis "$net/genesis.json" --sign-threads 4 --presign-s "$((dur + 5))" \
      --out "$TMP/ac3_load.json" >/dev/null 2>"$TMP/ac3_load.stderr" &
  local load_pid=$! pids="" t
  for i in 0 1 2 3; do pids="$pids $(cat "$net/node$i/hlnode.pid")"; done
  # client-side signing is not the chain's cost: the whole run is pre-signed before
  # sending starts (TEST_PLAN AC3 "pre-signs order traffic"); wait for the first
  # committed order actions, then sample per-thread CPU over the measurement window.
  for t in $(seq 1 600); do
    awk '$3 > 0 {f=1} END {exit !f}' "$net/node0/roots.log" 2>/dev/null && break
    kill -0 "$load_pid" 2>/dev/null || break
    sleep 1
  done
  log "load is committing; measuring"
  sleep $((warm + 5))
  python3 acceptance/thread_cpu.py "$((win - 10))" "$TMP/ac3_threads.json" $pids $load_pid >"$TMP/ac3_threads.txt" 2>&1 || true
  wait "$load_pid" || log "sim load exited non-zero"
  cat "$TMP/ac3_threads.txt"
  sleep 3
  scripts/localnet.sh status || true
  scripts/localnet.sh stop >/dev/null
  python3 acceptance/ac3_measure.py "$net/node0" "$warm" "$win" "$TMP/ac3_load.json" "$OUT" "$FINGERPRINT" \
      "$(GIT_DIRTY)" "$(git rev-parse HEAD)" "$MODE" "$min_rate"
}

# ---- AC5: SDK compatibility --------------------------------------------------------
run_ac5() {
  start_evidence
  build_release
  require_free_ports 3001 3002 3003 3004 26600 26601 26602 26603
  local py_json="$TMP/ac5_python.json" ts_json="$TMP/ac5_ts.json" prc trc
  AC5_NET="$ROOT/run/localnet"; local net="$AC5_NET"
  export LOCALNET_DIR="$net"
  trap 'scripts/localnet.sh stop >/dev/null 2>&1; rm -rf "$AC5_NET"' EXIT
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
