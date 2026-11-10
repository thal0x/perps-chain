#!/usr/bin/env bash
# AC5 (TypeScript): run the @nktkas/hyperliquid README / docs examples saved in
# acceptance/sdk/ts/ against a running, freshly seeded localnet.
#
#   acceptance/sdk/run_ts.sh [OUT_JSON]
#
# * deps (pinned in ts/package.json + package-lock.json: @nktkas/hyperliquid 0.33.3,
#   viem 2.57.8) are installed with `npm ci` OUTSIDE the repo, in $HL_TS_DEPS
#   (default /home/user/.cache/hlbft-acc-ts); the scripts are copied next to them.
# * check_ts_verbatim.py first asserts each example region is the upstream snippet
#   (clone $HL_TS_SDK_DIR, default /home/user/ext/hyperliquid) with only the documented
#   substitutions (see the header of each ts/*.ts and acceptance/README.md).
# * order: 01_read_data, 02_trading, 04_usd_send, then 06_subscriptions (>= 30 s
#   window); once it is subscribed, 03_order_modify_cancel and 05_market_order run (their
#   orders/fills are the events the orderUpdates / userFills channels must deliver).
# * verdict per script = exit 0 and the harness ACC_RESULT pass (ts/_harness.mjs).
# Env: LOCALNET_DIR, WS_SECS (default 40), SCRIPT_TIMEOUT (default 120).
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
DEPS="${HL_TS_DEPS:-/home/user/.cache/hlbft-acc-ts}"
CLONE="${HL_TS_SDK_DIR:-/home/user/ext/hyperliquid}"
NET="${LOCALNET_DIR:-$ROOT/run/localnet}"
URL="http://127.0.0.1:3001"
WS_SECS="${WS_SECS:-40}"
SCRIPT_TIMEOUT="${SCRIPT_TIMEOUT:-120}"
OUT="${1:-$ROOT/acceptance/tmp/ts.json}"
die() { echo "run_ts: $*" >&2; exit 2; }

command -v node >/dev/null || die "node not found"
curl -sf -m 3 "$URL/admin/status" >/dev/null || die "no node API at $URL"
[ -f "$NET/accounts.json" ] || die "no $NET/accounts.json"

# ---- deps (outside the repo) ---------------------------------------------------
mkdir -p "$DEPS"
if ! cmp -s "$HERE/ts/package-lock.json" "$DEPS/package-lock.json" || [ ! -d "$DEPS/node_modules/@nktkas/hyperliquid" ]; then
  echo "== npm ci in $DEPS"
  cp "$HERE/ts/package.json" "$HERE/ts/package-lock.json" "$DEPS/"
  (cd "$DEPS" && npm ci --no-audit --no-fund --silent) || die "npm ci failed"
fi
SDK_VER="$(node -p "require('$DEPS/node_modules/@nktkas/hyperliquid/package.json').version")"
VIEM_VER="$(node -p "require('$DEPS/node_modules/viem/package.json').version")"
[ "$SDK_VER" = "0.33.3" ] || die "installed @nktkas/hyperliquid $SDK_VER != pinned 0.33.3"
CLONE_VER="$(python3 -c "import json;print(json.load(open('$CLONE/deno.json'))['version'])")"
[ "$CLONE_VER" = "$SDK_VER" ] || die "snippet source clone is $CLONE_VER, npm package is $SDK_VER"
CLONE_REV="$(git -C "$CLONE" rev-parse HEAD 2>/dev/null || echo unknown)"

# ---- harness self-test (D27 uncaught-final-throw rule) -------------------------
echo "== harness self-test"
HL_TS_DEPS="$DEPS" "$HERE/selftest/ts_harness_selftest.sh" | sed 's/^/   /'
[ "${PIPESTATUS[0]}" = 0 ] || die "harness self-test failed"

# ---- verbatim check ------------------------------------------------------------
echo "== verbatim check (snippet source $CLONE @ $CLONE_REV, v$CLONE_VER)"
VERB="$(python3 "$HERE/check_ts_verbatim.py" "$HERE/ts" "$CLONE")" || { echo "$VERB"; die "verbatim check failed"; }
echo "$VERB" | python3 -c 'import json,sys; d=json.load(sys.stdin); [print(f"   ok  {r["file"]:<24} {r["source"]}  ({r["substitutions"]} substitutions)") for r in d["regions"]]'

RUN="$DEPS/run"
rm -rf "$RUN"; mkdir -p "$RUN/out"
cp "$HERE"/ts/*.ts "$HERE"/ts/*.mjs "$RUN/"
cp "$HERE/ts/package.json" "$RUN/"
ln -s "$DEPS/node_modules" "$RUN/node_modules"
trap 'rm -rf "$RUN"' EXIT

eval "$(python3 - "$NET/accounts.json" <<'EOF'
import json, sys
a = [x for x in json.load(open(sys.argv[1])) if x.get("name") == "sdk_example"][0]
print(f'export HL_ACC_KEY={a["secret"]}; export HL_ACC_ADDR={a["address"]}')
EOF
)"
export HL_ACC_ALLOWLIST="$HERE/expected_errors.json" WS_SECS
export NODE_NO_WARNINGS=1

RESULTS="$RUN/out/results.jsonl"; : >"$RESULTS"
record() { # name rc logfile [elapsed]
  python3 - "$1" "$2" "$3" "${4:-0}" >>"$RESULTS" <<'EOF'
import json, sys
name, rc, log, el = sys.argv[1], int(sys.argv[2]), sys.argv[3], float(sys.argv[4])
res = None
for line in open(log, errors="replace"):
    if line.startswith("ACC_RESULT "):
        res = json.loads(line[len("ACC_RESULT "):])
v = {"script": name, "rc": rc, "elapsed_s": round(el, 1)}
if res is None:
    v.update(pass_=False, reasons=[f"no ACC_RESULT (rc={rc})"])
else:
    v.update(res); v["script"] = name
    if rc != 0 and not any("exit code" in r for r in v["reasons"]):
        v["reasons"].append(f"exit code {rc}")
    v["pass_"] = bool(res["pass"]) and rc == 0
v["pass"] = v.pop("pass_")
tail = [l.rstrip() for l in open(log, errors="replace") if not l.startswith(("ACC_CALL", "ACC_RESULT"))][-8:]
if not v["pass"]:
    v["log_tail"] = tail
print(json.dumps(v))
EOF
  tail -n1 "$RESULTS" | python3 -c 'import json,sys; v=json.load(sys.stdin); print("   =>", "PASS" if v["pass"] else "FAIL", "; ".join(v["reasons"]))'
}
show() { grep -v '^ACC_RESULT' "$1" | cut -c1-300 | sed 's/^/   | /' | head -30; }

run_one() { # name
  local n="$1" t0 rc
  echo "== $n"
  t0=$(date +%s.%N)
  (cd "$RUN" && HL_ACC_SCRIPT="$n" timeout "$SCRIPT_TIMEOUT" node "$n.ts") >"$RUN/out/$n.log" 2>&1
  rc=$?
  show "$RUN/out/$n.log"
  record "$n" "$rc" "$RUN/out/$n.log" "$(echo "$(date +%s.%N) - $t0" | bc)"
}

run_one 01_read_data
run_one 02_trading
run_one 04_usd_send

echo "== 06_subscriptions (${WS_SECS}s window; 03 + 05 run once subscribed)"
t0=$(date +%s.%N)
(cd "$RUN" && HL_ACC_SCRIPT=06_subscriptions timeout $((WS_SECS + 60)) node 06_subscriptions.ts) >"$RUN/out/06_subscriptions.log" 2>&1 &
spid=$!
for _ in $(seq 1 60); do grep -q '^ACC_SUBSCRIBED' "$RUN/out/06_subscriptions.log" 2>/dev/null && break; kill -0 $spid 2>/dev/null || break; sleep 0.5; done
run_one 03_order_modify_cancel
run_one 05_market_order
wait $spid; src=$?
show "$RUN/out/06_subscriptions.log"
record 06_subscriptions "$src" "$RUN/out/06_subscriptions.log" "$(echo "$(date +%s.%N) - $t0" | bc)"

mkdir -p "$(dirname "$OUT")"
python3 - "$RESULTS" "$OUT" "$SDK_VER" "$VIEM_VER" "$CLONE_REV" "$VERB" <<'EOF'
import json, sys
res = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
order = ["01_read_data", "02_trading", "03_order_modify_cancel", "04_usd_send", "05_market_order", "06_subscriptions"]
res.sort(key=lambda r: order.index(r["script"]))
out = {"sdk": "@nktkas/hyperliquid", "sdk_version": sys.argv[3], "viem_version": sys.argv[4],
       "snippet_source_commit": sys.argv[5], "verbatim_check": json.loads(sys.argv[6]),
       "transport_changes": {"HttpTransport": '{ apiUrl: "http://127.0.0.1:3001", isTestnet: true }',
                             "WebSocketTransport": '{ url: "ws://127.0.0.1:3001/ws", isTestnet: true }'},
       "passed": sum(r["pass"] for r in res), "failed": sum(not r["pass"] for r in res), "scripts": res}
json.dump(out, open(sys.argv[2], "w"), indent=1)
print(f"== ts examples: {out['passed']} passed, {out['failed']} failed")
for r in res:
    print(f"   {'PASS' if r['pass'] else 'FAIL'}  {r['script']}  {'; '.join(r['reasons'])}")
sys.exit(0 if out["failed"] == 0 else 1)
EOF
