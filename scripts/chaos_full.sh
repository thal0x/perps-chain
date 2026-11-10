#!/usr/bin/env bash
# AC1 full run (TEST_PLAN AC1), invoked by `scripts/acceptance.sh ac1` (full mode).
#   7 validators, 12 h, max-faulty 2, all episode kinds (20-120 s episodes, 20 s
#   observation), background load from `sim`, as chaos_smoke.sh but full length.
# Writes acceptance/results/ac1.json from hlchaos' report. Refuses a dirty tree
# (fingerprinted sources + this script + acceptance.sh), like the other runners.
# Partial reports (hlchaos flushes them periodically) are copied to
# acceptance/tmp/ac1_partial.json every 5 min, so a crash leaves evidence of what ran.
# Env: AC1_DURATION (12h) AC1_SEED (20261110) AC1_LOAD_RATE (5) AC1_OUT (evidence path) ALLOW_DIRTY.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
DURATION="${AC1_DURATION:-12h}"
SEED="${AC1_SEED:-20261110}"
LOAD_RATE="${AC1_LOAD_RATE:-5}"
OUT="${AC1_OUT:-$ROOT/acceptance/results/ac1.json}"
TMP="$ROOT/acceptance/tmp"; mkdir -p "$TMP"
log() { echo "[ac1 full $(date -u +%H:%M:%S)] $*"; }
die() { echo "ac1 full: $*" >&2; exit 1; }

FP="$(python3 scripts/evidence.py --fingerprint)"
DIRTY="$(git status --porcelain -- crates bridge Cargo.lock acceptance/sdk scripts/acceptance.sh scripts/chaos_full.sh 2>/dev/null)"
if [ -n "$DIRTY" ] && [ "${ALLOW_DIRTY:-0}" != 1 ]; then
  echo "$DIRTY" >&2; die "uncommitted changes in fingerprinted sources: commit first"
fi
HEAD="$(git rev-parse HEAD)"

log "cargo build --release -p node -p chaos -p sim"
cargo build -q --release -p node -p chaos -p sim || die "release build failed"
TDIR="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])')"
BIN="$ROOT/run/bin_ac1"; mkdir -p "$BIN"
cp "$TDIR/release/hlnode" "$TDIR/release/hlchaos" "$TDIR/release/sim" "$BIN/"   # private copies
WORK="$ROOT/run/ac1"; rm -rf "$WORK"; mkdir -p "$WORK"
REPORT="$WORK/report.json"
SIM_ARGS=()
"$BIN/sim" load --help >/dev/null 2>&1 && SIM_ARGS=(--sim "$BIN/sim")

log "hlchaos run --validators 7 --duration $DURATION --max-faulty 2 --seed $SEED (load $LOAD_RATE tx/s)"
"$BIN/hlchaos" run --validators 7 --duration "$DURATION" --max-faulty 2 --seed "$SEED" \
  --hlnode "$BIN/hlnode" --workdir "$WORK" --load-rate "$LOAD_RATE" --kinds all \
  --report "$REPORT" "${SIM_ARGS[@]}" >"$WORK/hlchaos.log" 2>&1 &
CPID=$!
trap 'kill "$CPID" 2>/dev/null' INT TERM
while kill -0 "$CPID" 2>/dev/null; do
  for _ in $(seq 1 60); do kill -0 "$CPID" 2>/dev/null || break; sleep 5; done
  [ -f "$REPORT" ] && cp "$REPORT" "$TMP/ac1_partial.json"
  tail -n 1 "$WORK/hlchaos.log" 2>/dev/null | cut -c1-200
done
wait "$CPID"; rc=$?
[ -f "$REPORT" ] && cp "$REPORT" "$TMP/ac1_partial.json"
[ -f "$REPORT" ] || { tail -30 "$WORK/hlchaos.log" >&2; die "hlchaos produced no report (rc $rc)"; }

python3 - "$REPORT" "$OUT" "$FP" "$HEAD" "$rc" "$([ -n "$DIRTY" ] && echo true || echo false)" <<'EOF'
import json, sys, datetime
rep, out, fp, head, rc, dirty = sys.argv[1:]
r = json.load(open(rep))
qc = r.get("qc") or {}
qf = qc.get("failures", None)
qc_failures = len(qf) if isinstance(qf, list) else (qf if isinstance(qf, int) else 1)
ok = bool(r.get("pass")) and rc == "0" and r.get("complete", False) and qc_failures == 0 and (qc.get("blocks_verified") or 0) > 0
ev = {
    "ac": "ac1", "mode": "full", "pass": ok,
    "duration_s": r["duration_s"], "validators": r["validators"], "max_faulty": r.get("max_faulty"),
    "episodes": r["episodes"], "heals": r["heals"],
    "max_resume_ms": r["max_resume_ms"], "p50_resume_ms": r.get("p50_resume_ms"), "p99_resume_ms": r.get("p99_resume_ms"),
    "safety_violations": r["safety_violations"], "kills": r.get("kills"), "partitions": r.get("partitions"),
    "splits": r.get("splits"), "lossy_episodes": r.get("lossy_episodes"), "progress_stalls": r.get("progress_stalls"),
    "unexpected_exits": r.get("unexpected_exits"),
    "qc_verified_blocks": qc.get("blocks_verified"), "qc_failures": qc_failures, "qc_failure_details": (qf if isinstance(qf, list) else [])[:20], "qc_nodes_checked": qc.get("nodes_checked"),
    "final_height": r.get("final_height"), "seed": r["seed"], "episodes_by_kind": r.get("episodes_by_kind"),
    "violations": (r.get("violations") or [])[:20], "checker": r.get("checker"), "load": r.get("load"),
    "complete": r.get("complete"), "hlchaos_exit": int(rc),
    "source_fingerprint": fp, "git_head": head, "git_dirty": dirty == "true",
    "time_utc": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "summary": r.get("summary", ""),
}
json.dump(ev, open(out, "w"), indent=1)
print(f"AC1 {'PASS' if ok else 'FAIL'}: {ev['summary']}  -> {out}")
sys.exit(0 if ok else 1)
EOF
