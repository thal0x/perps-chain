#!/usr/bin/env bash
# AC6 bridge end-to-end (TEST_PLAN AC6, D18).
#
#   acceptance/bridge_e2e.sh [OUT_JSON] [full|smoke]      (scripts/acceptance.sh ac6 calls this)
#
#  1. anvil --block-time 1 on :8545
#  2. hlnode testnet-gen (seed 42) -> validators.json; bridge/deploy.sh with
#     VALIDATORS/POWERS = those validators/stakes; testnet-gen again with
#     --bridge-contract <deployed Bridge> (same seed => same validators, checked)
#  3. 4-validator localnet, every node with --bridge-rpc (DepositWatcher); seed ladder
#     (scripts/localnet.sh seed); `hlrelayer relay` (Anvil dev account 9 pays gas)
#  4. user = the SDK example account (key in accounts.json): funded with 1 ETH (gas) and
#     1000 MockUSDC (mint) on Anvil; approve + deposit 1000 USDC with `cast send`
#  5. wait until clearinghouseState accountValue shows exactly +1000
#  6. trade: Python SDK IoC buy 0.01 ETH that fills against the seeded MM ask
#  7. Python SDK exchange.withdraw_from_bridge(500, user)  (withdraw3)
#  8. wait for the relayer's withdrawSubmitted; assert Anvil USDC balance +499 exactly
#  9. forge test (bridge/), counts + required test families
# Env: HLNODE, HLRELAYER (binaries; default run/bin/*), HL_PYTHON, SOURCE_FINGERPRINT,
#      GIT_DIRTY_FLAG, DEPOSIT_TIMEOUT (120), WITHDRAW_TIMEOUT (180).
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
OUT="${1:-$ROOT/acceptance/tmp/ac6.json}"
MODE="${2:-full}"
HLNODE="${HLNODE:-$ROOT/run/bin/hlnode}"
HLRELAYER="${HLRELAYER:-$ROOT/run/bin/hlrelayer}"
PY="${HL_PYTHON:-/home/user/.venvs/hl/bin/python}"
FP="${SOURCE_FINGERPRINT:-$(python3 scripts/evidence.py --fingerprint)}"
RPC="http://127.0.0.1:8545"
API="http://127.0.0.1:3001"
NET="$ROOT/run/bridge_e2e"
W="$NET/work"
DEPLOY_NAME="acceptance_e2e"
DEPLOYMENT="$ROOT/bridge/deployments/$DEPLOY_NAME.json"
DEV0_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"   # Anvil dev account 0 (public test key)
DEPOSIT_TIMEOUT="${DEPOSIT_TIMEOUT:-120}"
WITHDRAW_TIMEOUT="${WITHDRAW_TIMEOUT:-180}"

for b in "$HLNODE" "$HLRELAYER" "$PY"; do [ -x "$b" ] || { echo "bridge_e2e: missing $b" >&2; exit 1; }; done
for t in anvil forge cast; do command -v $t >/dev/null || { echo "bridge_e2e: $t not on PATH" >&2; exit 1; }; done

rm -rf "$NET"; mkdir -p "$W"
STEPS="$W/steps.jsonl"; : >"$STEPS"
ANVIL_PID=""; RELAYER_PID=""
cleanup() {
  [ -n "$RELAYER_PID" ] && kill "$RELAYER_PID" 2>/dev/null
  LOCALNET_DIR="$NET" scripts/localnet.sh stop >/dev/null 2>&1
  [ -n "$ANVIL_PID" ] && kill "$ANVIL_PID" 2>/dev/null
  rm -f "$DEPLOYMENT"
  if [ "${KEEP_RUN:-0}" != 1 ]; then rm -rf "$NET"; fi
}
trap cleanup EXIT

log() { echo "[bridge_e2e $(date -u +%H:%M:%S)] $*"; }
step() { # name ok(true|false) json-details (invalid JSON is stored as a string)
  python3 - "$1" "$2" "${3:-null}" >>"$STEPS" <<'EOF'
import json, sys
name, ok, det = sys.argv[1:]
try: det = json.loads(det)
except ValueError: pass
print(json.dumps({"step": name, "ok": ok == "true", "details": det}))
EOF
  log "step $1: $([ "$2" = true ] && echo ok || echo FAILED)"
}
E2E_OK=true
fail() { E2E_OK=false; step "$1" false "${2:-null}"; finish; }

usdc_raw() { cast call "$USDC" "balanceOf(address)(uint256)" "$1" --rpc-url "$RPC" | awk '{print $1}'; }

# L1 helpers (Python SDK Info)
l1_account_value() {
  "$PY" - "$API" "$1" <<'EOF'
import sys
from hyperliquid.info import Info
s = Info(sys.argv[1], skip_ws=True).user_state(sys.argv[2])
print(s["marginSummary"]["accountValue"])
EOF
}

FORGE_PASSED=0; FORGE_FAILED=1; FORGE_REQ="{}"
run_forge() {
  log "forge test (bridge/)"
  (cd bridge && forge test) >"$W/forge.log" 2>&1
  local rc=$?
  read -r FORGE_PASSED FORGE_FAILED < <(python3 - "$W/forge.log" <<'EOF'
import re, sys
t = open(sys.argv[1], errors="replace").read()
m = re.findall(r"(\d+) tests passed, (\d+) failed", t)
print(*(m[-1] if m else ("0", "1")))
EOF
)
  [ "$rc" -ne 0 ] && [ "$FORGE_FAILED" -eq 0 ] && FORGE_FAILED=1
  FORGE_REQ=$(python3 - "$W/forge.log" <<'EOF'
import json, re, sys
t = open(sys.argv[1], errors="replace").read()
out = {}
for fam in ["test_ReplayProtection", "test_SignatureThreshold", "test_ValidatorSetRotation"]:
    out[fam] = {"passed": len(re.findall(r"\[PASS\] " + fam, t)), "failed": len(re.findall(r"\[FAIL[^\]]*\] " + fam, t))}
print(json.dumps(out))
EOF
)
  tail -3 "$W/forge.log"
}

finish() {
  [ "$FORGE_DONE" = 1 ] || { run_forge; FORGE_DONE=1; }
  mkdir -p "$(dirname "$OUT")"
  python3 - "$STEPS" "$OUT" "$E2E_OK" "$FORGE_PASSED" "$FORGE_FAILED" "$FORGE_REQ" "$FP" "${GIT_DIRTY_FLAG:-false}" "$MODE" "$W/facts.json" <<'EOF'
import json, os, sys, datetime, subprocess
steps_p, out, e2e, fp_, ff, freq, fp, dirty, mode, facts_p = sys.argv[1:]
steps = [json.loads(l) for l in open(steps_p) if l.strip()]
facts = json.load(open(facts_p)) if os.path.exists(facts_p) else {}
e2e_ok = e2e == "true" and all(s["ok"] for s in steps) and any(s["step"] == "anvil_balance_plus_499" and s["ok"] for s in steps)
freq = json.loads(freq)
req_ok = all(v["passed"] > 0 and v["failed"] == 0 for v in freq.values()) if freq else False
fp_, ff = int(fp_), int(ff)
ok = e2e_ok and ff == 0 and fp_ > 0 and req_ok
head = subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
failed = [s["step"] for s in steps if not s["ok"]]
ev = {"ac": "ac6", "mode": mode, "pass": ok, "e2e_pass": e2e_ok,
      "forge_passed": fp_, "forge_failed": ff, "forge_required_families": freq,
      "source_fingerprint": fp, "git_head": head, "git_dirty": dirty == "true",
      "time_utc": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
      "summary": (f"e2e {'ok' if e2e_ok else 'FAILED at ' + ','.join(failed)}; "
                  f"anvil delta {facts.get('anvil_usdc_delta', '?')} USDC; forge {fp_} passed / {ff} failed"),
      **facts, "steps": steps}
json.dump(ev, open(out, "w"), indent=1)
print(f"AC6 {'PASS' if ok else 'FAIL'}: {ev['summary']} -> {out}")
sys.exit(0 if ok else 1)
EOF
  exit $?
}
FORGE_DONE=0
FACTS="$W/facts.json"; echo '{}' >"$FACTS"
fact() { python3 - "$FACTS" "$1" "$2" <<'EOF'
import json, sys
p, k, v = sys.argv[1:]
d = json.load(open(p))
try: d[k] = json.loads(v)
except ValueError: d[k] = v
json.dump(d, open(p, "w"), indent=1)
EOF
}

# ---- 1. anvil -------------------------------------------------------------------
log "anvil --block-time 1"
anvil --block-time 1 --port 8545 --silent >"$W/anvil.log" 2>&1 &
ANVIL_PID=$!
for _ in $(seq 1 50); do cast block-number --rpc-url "$RPC" >/dev/null 2>&1 && break; sleep 0.2; done
cast block-number --rpc-url "$RPC" >/dev/null 2>&1 || fail anvil_start
step anvil_start true "{\"rpc\": \"$RPC\", \"block_time_s\": 1}"

# ---- 2. validators + deploy + genesis with the bridge address --------------------
GEN="$NET/gen0"
"$HLNODE" testnet-gen --validators 4 --out "$GEN" --accounts 10 --seed 42 --fund-sdk-examples >/dev/null || fail testnet_gen
VALS=$(python3 -c "import json;print(','.join(v['address'] for v in json.load(open('$GEN/validators.json'))))")
POWS=$(python3 -c "import json;print(','.join(str(v['stake']) for v in json.load(open('$GEN/validators.json'))))")
log "deploy Bridge with VALIDATORS=$VALS POWERS=$POWS"
DEPLOY_OUT=$(VALIDATORS="$VALS" POWERS="$POWS" MINT_AMOUNT=0 DEPLOY_NAME="$DEPLOY_NAME" RPC_URL="$RPC" bridge/deploy.sh 2>"$W/deploy.err") \
  || { cat "$W/deploy.err" >&2; fail deploy; }
echo "$DEPLOY_OUT" >"$W/deployment.json"
BRIDGE=$(python3 -c "import json;print(json.load(open('$DEPLOYMENT'))['bridge'])")
USDC=$(python3 -c "import json;print(json.load(open('$DEPLOYMENT'))['usdc'])")
step deploy true "$(cat "$DEPLOYMENT")"
fact bridge "\"$BRIDGE\""; fact usdc "\"$USDC\""; fact validators "$(python3 -c "import json;print(json.dumps(json.load(open('$DEPLOYMENT'))['validators']))")"

rm -rf "$NET/node"* "$NET/genesis.json"
"$HLNODE" testnet-gen --validators 4 --out "$NET" --accounts 10 --seed 42 --fund-sdk-examples --bridge-contract "$BRIDGE" >/dev/null || fail testnet_gen_bridge
python3 - "$GEN/validators.json" "$NET/validators.json" "$NET/genesis.json" "$BRIDGE" <<'EOF' || fail genesis_check
import json, sys
a, b = json.load(open(sys.argv[1])), json.load(open(sys.argv[2]))
assert [v["address"] for v in a] == [v["address"] for v in b], "validator set changed between testnet-gen runs"
g = json.load(open(sys.argv[3]))
br = g["bridge"]
assert br["contract"].lower() == sys.argv[4].lower(), f"genesis bridge.contract {br['contract']} != {sys.argv[4]}"
assert int(br["eth_chain_id"]) == 31337, br
print("genesis bridge:", json.dumps(br))
EOF
step genesis true "{\"bridge_contract\": \"$BRIDGE\", \"eth_chain_id\": 31337}"

# ---- 3. localnet with --bridge-rpc + seed + relayer --------------------------------
for i in 0 1 2 3; do
  nohup "$HLNODE" run --home "$NET/node$i" --bridge-rpc "$RPC" >"$NET/node$i/stdout.log" 2>&1 &
  echo $! >"$NET/node$i/hlnode.pid"
done
ok=0
for _ in $(seq 1 120); do
  n=0
  for i in 0 1 2 3; do
    h=$(curl -s -m 2 "http://127.0.0.1:$((3001 + i))/admin/status" | python3 -c 'import sys,json
try: print(json.load(sys.stdin)["committed_height"])
except Exception: print(-1)')
    [ "$h" -gt 2 ] 2>/dev/null && n=$((n + 1))
  done
  [ "$n" = 4 ] && { ok=1; break; }
  sleep 0.5
done
[ "$ok" = 1 ] || { LOCALNET_DIR="$NET" scripts/localnet.sh status; fail localnet_start; }
LOCALNET_DIR="$NET" scripts/localnet.sh status
step localnet_start true '{"validators": 4, "bridge_rpc": "http://127.0.0.1:8545"}'
SEED_OUT=$(LOCALNET_DIR="$NET" BASE_API=3001 scripts/localnet.sh seed 2>&1) || fail seed "$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$SEED_OUT")"
echo "$SEED_OUT"
step seed true "$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$SEED_OUT")"

"$HLRELAYER" relay --api "$API" --rpc "$RPC" --deployment "$DEPLOYMENT" --dev-account 9 --poll-ms 1000 \
  >"$W/relayer.out" 2>"$W/relayer.err" &
RELAYER_PID=$!
sleep 1
kill -0 "$RELAYER_PID" 2>/dev/null || { cat "$W/relayer.err" >&2; fail relayer_start; }
step relayer_start true '{"sender": "anvil dev account 9"}'

# ---- 4. user funding + approve + deposit ---------------------------------------
eval "$(python3 - "$NET/accounts.json" <<'EOF'
import json, sys
a = [x for x in json.load(open(sys.argv[1])) if x.get("name") == "sdk_example"][0]
print(f'USER_KEY={a["secret"]}; USER_ADDR={a["address"]}')
EOF
)"
USER_ADDR=$(cast to-check-sum-address "$USER_ADDR")
fact user "\"$USER_ADDR\""
txh() { python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["status"] in ("0x1", 1, "1"), d; print(d["transactionHash"])'; }
ETH_TX=$(cast send "$USER_ADDR" --value 1ether --private-key "$DEV0_KEY" --rpc-url "$RPC" --json | txh) || fail fund_eth
MINT_TX=$(cast send "$USDC" "mint(address,uint256)" "$USER_ADDR" 1000000000 --private-key "$DEV0_KEY" --rpc-url "$RPC" --json | txh) || fail mint
B_MINTED=$(usdc_raw "$USER_ADDR")
[ "$B_MINTED" = 1000000000 ] || fail mint "{\"balance_raw\": \"$B_MINTED\"}"
step fund_user true "{\"eth_tx\": \"$ETH_TX\", \"mint_tx\": \"$MINT_TX\", \"usdc_raw\": \"$B_MINTED\"}"

AV0=$(l1_account_value "$USER_ADDR") || fail l1_query
fact l1_account_value_before_deposit "\"$AV0\""
APPROVE_TX=$(cast send "$USDC" "approve(address,uint256)" "$BRIDGE" 1000000000 --private-key "$USER_KEY" --rpc-url "$RPC" --json | txh) || fail approve
DEPOSIT_JSON=$(cast send "$BRIDGE" "deposit(uint256)" 1000000000 --private-key "$USER_KEY" --rpc-url "$RPC" --json) || fail deposit
DEPOSIT_TX=$(echo "$DEPOSIT_JSON" | txh) || fail deposit
DEPOSIT_BLOCK=$(echo "$DEPOSIT_JSON" | python3 -c 'import json,sys;print(int(json.load(sys.stdin)["blockNumber"],16))')
B_AFTER_DEPOSIT=$(usdc_raw "$USER_ADDR")
BRIDGE_BAL=$(usdc_raw "$BRIDGE")
[ "$B_AFTER_DEPOSIT" = 0 ] && [ "$BRIDGE_BAL" = 1000000000 ] || fail deposit "{\"user_raw\": \"$B_AFTER_DEPOSIT\", \"bridge_raw\": \"$BRIDGE_BAL\"}"
fact approve_tx "\"$APPROVE_TX\""; fact deposit_tx "\"$DEPOSIT_TX\""; fact deposit_block "$DEPOSIT_BLOCK"
step approve_deposit true "{\"approve_tx\": \"$APPROVE_TX\", \"deposit_tx\": \"$DEPOSIT_TX\", \"block\": $DEPOSIT_BLOCK, \"bridge_usdc_raw\": \"$BRIDGE_BAL\"}"

# ---- 5. wait for the L1 credit (+1000 exactly) ----------------------------------
log "waiting for clearinghouseState +1000 (<= ${DEPOSIT_TIMEOUT}s)"
t0=$(date +%s); AV1=""
while [ $(( $(date +%s) - t0 )) -lt "$DEPOSIT_TIMEOUT" ]; do
  AV1=$(l1_account_value "$USER_ADDR" 2>/dev/null)
  if python3 -c "from decimal import Decimal as D;import sys;sys.exit(0 if D('$AV1')-D('$AV0')>=1000 else 1)" 2>/dev/null; then break; fi
  sleep 1
done
CREDIT_S=$(( $(date +%s) - t0 ))
DELTA=$(python3 -c "from decimal import Decimal as D;print(D('$AV1')-D('$AV0'))")
fact l1_account_value_after_deposit "\"$AV1\""; fact l1_deposit_credit "\"$DELTA\""; fact deposit_credit_s "$CREDIT_S"
python3 -c "from decimal import Decimal as D;import sys;sys.exit(0 if D('$DELTA')==1000 else 1)" \
  || fail l1_credit_plus_1000 "{\"before\": \"$AV0\", \"after\": \"$AV1\", \"delta\": \"$DELTA\", \"waited_s\": $CREDIT_S}"
step l1_credit_plus_1000 true "{\"before\": \"$AV0\", \"after\": \"$AV1\", \"delta\": \"$DELTA\", \"waited_s\": $CREDIT_S}"

# ---- 6 + 7. trade, then withdraw3 500 via the Python SDK ---------------------------
log "trade (IoC buy 0.01 ETH against the MM ask) + withdraw_from_bridge(500)"
"$PY" - "$API" "$USER_KEY" "$USER_ADDR" "$W/l1.json" <<'EOF'
import json, sys
import eth_account
from hyperliquid.exchange import Exchange
from hyperliquid.info import Info
url, key, addr, outp = sys.argv[1:]
acct = eth_account.Account.from_key(key)
info = Info(url, skip_ws=True)
ex = Exchange(acct, url, account_address=addr)
out = {}
book = info.l2_snapshot("ETH")
best_ask = book["levels"][1][0]
out["best_ask"] = best_ask
mm = [o for o in info.open_orders(addr)]  # the user's own orders (expect none)
r = ex.order("ETH", True, 0.01, float(best_ask["px"]), {"limit": {"tif": "Ioc"}})
out["order_response"] = r
st = r["response"]["data"]["statuses"][0] if r.get("status") == "ok" else {}
out["trade_ok"] = "filled" in st and st["filled"]["totalSz"] == "0.01"
fills = info.user_fills(addr)
out["fills"] = [f for f in fills if f["coin"] == "ETH"][:3]
out["fill_hash"] = out["fills"][0]["hash"] if out["fills"] else None
out["account_value_after_trade"] = info.user_state(addr)["marginSummary"]["accountValue"]
w = ex.withdraw_from_bridge(500, addr)
out["withdraw_response"] = w
out["withdraw_ok"] = w.get("status") == "ok"
out["account_value_after_withdraw"] = info.user_state(addr)["marginSummary"]["accountValue"]
pend = info.post("/info", {"type": "pendingWithdrawals"})
out["pending_withdrawals"] = pend
json.dump(out, open(outp, "w"), indent=1)
print(json.dumps({k: out[k] for k in ("trade_ok", "withdraw_ok", "fill_hash")}))
print("order:", json.dumps(r))
print("withdraw:", json.dumps(w))
print("pending:", json.dumps(pend))
EOF
[ -f "$W/l1.json" ] || fail trade_withdraw
python3 -c "import json;d=json.load(open('$W/l1.json'));import sys;sys.exit(0 if d['trade_ok'] else 1)" \
  || fail trade "$(python3 -c "import json;d=json.load(open('$W/l1.json'));print(json.dumps(d['order_response']))")"
step trade true "$(python3 -c "import json;d=json.load(open('$W/l1.json'));print(json.dumps({'order_response':d['order_response'],'fill_hash':d['fill_hash'],'best_ask':d['best_ask']}))")"
fact trade_fill_hash "$(python3 -c "import json;print(json.dumps(json.load(open('$W/l1.json'))['fill_hash']))")"
python3 -c "import json;d=json.load(open('$W/l1.json'));import sys;sys.exit(0 if d['withdraw_ok'] else 1)" \
  || fail withdraw3 "$(python3 -c "import json;d=json.load(open('$W/l1.json'));print(json.dumps(d['withdraw_response']))")"
PEND=$(python3 - "$W/l1.json" "$USER_ADDR" <<'EOF'
import json, sys
d = json.load(open(sys.argv[1]))
p = d["pending_withdrawals"]
p = p.get("pendingWithdrawals", p) if isinstance(p, dict) else p
mine = [w for w in p if str(w.get("destination", "")).lower() == sys.argv[2].lower()]
print(json.dumps(mine))
EOF
)
step withdraw3 true "{\"withdraw_response\": $(python3 -c "import json;print(json.dumps(json.load(open('$W/l1.json'))['withdraw_response']))"), \"pending\": $PEND, \"account_value_after_trade\": \"$(python3 -c "import json;print(json.load(open('$W/l1.json'))['account_value_after_trade'])")\", \"account_value_after_withdraw\": \"$(python3 -c "import json;print(json.load(open('$W/l1.json'))['account_value_after_withdraw'])")\"}"
fact pending_withdrawal "$PEND"
python3 - "$PEND" <<'EOF' || fail pending_withdrawal_net_499 "$PEND"
import json, sys
from decimal import Decimal
p = json.loads(sys.argv[1])
assert len(p) == 1, p
a = p[0]["amount"]
assert Decimal(str(a)) == Decimal("499"), f"pending amount {a!r} != 499 (net of 1 USDC fee)"
EOF
step pending_withdrawal_net_499 true "$PEND"

# ---- 8. relayer submits; Anvil balance +499 ----------------------------------------
B0="$B_AFTER_DEPOSIT"
fact anvil_usdc_before_withdraw_raw "\"$B0\""
log "waiting for relayer withdrawSubmitted + Anvil balance (<= ${WITHDRAW_TIMEOUT}s)"
t0=$(date +%s); B1="$B0"; WTX=""
while [ $(( $(date +%s) - t0 )) -lt "$WITHDRAW_TIMEOUT" ]; do
  WTX=$(python3 -c "
import json
for l in open('$W/relayer.out'):
    try: e=json.loads(l)
    except ValueError: continue
    if e.get('event')=='withdrawSubmitted': print(e['txHash']); break
" 2>/dev/null)
  B1=$(usdc_raw "$USER_ADDR")
  [ -n "$WTX" ] && [ "$B1" != "$B0" ] && break
  kill -0 "$RELAYER_PID" 2>/dev/null || break
  sleep 1
done
RELAY_S=$(( $(date +%s) - t0 ))
sleep 3   # let any (incorrect) second payout land before the final balance check
B1=$(usdc_raw "$USER_ADDR")
D_RAW=$(( B1 - B0 ))
fact anvil_usdc_after_withdraw_raw "\"$B1\""
fact anvil_usdc_delta "$(python3 -c "import json;from decimal import Decimal as D;print(json.dumps(str(D($D_RAW)/D(10**6))))")"
fact withdraw_tx "\"$WTX\""; fact relay_wait_s "$RELAY_S"
fact relayer_events "$(python3 -c "
import json
ev=[]
for l in open('$W/relayer.out'):
    try: ev.append(json.loads(l))
    except ValueError: pass
print(json.dumps(ev))")"
if [ -n "$WTX" ]; then
  RCPT=$(cast receipt "$WTX" --rpc-url "$RPC" --json 2>/dev/null | python3 -c 'import json,sys;d=json.load(sys.stdin);print(json.dumps({"status":d["status"],"block":int(d["blockNumber"],16),"gasUsed":int(d["gasUsed"],16)}))')
  fact withdraw_receipt "$RCPT"
fi
[ -n "$WTX" ] || { tail -20 "$W/relayer.err" >&2; fail relayer_submitted "{\"waited_s\": $RELAY_S}"; }
step relayer_submitted true "{\"tx\": \"$WTX\", \"waited_s\": $RELAY_S}"
[ "$D_RAW" = 499000000 ] || fail anvil_balance_plus_499 "{\"before_raw\": \"$B0\", \"after_raw\": \"$B1\", \"delta_raw\": $D_RAW}"
step anvil_balance_plus_499 true "{\"before_raw\": \"$B0\", \"after_raw\": \"$B1\", \"delta_raw\": $D_RAW}"
USED=$(cast call "$BRIDGE" "usedWithdrawal(uint64)(bool)" "$(echo "$PEND" | python3 -c 'import json,sys;print(json.load(sys.stdin)[0]["id"])')" --rpc-url "$RPC")
fact bridge_used_withdrawal "\"$USED\""
fact bridge_usdc_after_raw "\"$(usdc_raw "$BRIDGE")\""

# L1 finalization by the watchers (recorded, not part of the AC pass condition)
FIN=false
for _ in $(seq 1 30); do
  n=$(curl -s -m 2 "$API/info" -H 'content-type: application/json' -d '{"type":"pendingWithdrawals"}' | python3 -c "
import json,sys
p=json.load(sys.stdin); p=p.get('pendingWithdrawals',p) if isinstance(p,dict) else p
print(len([w for w in p if str(w.get('destination','')).lower()=='${USER_ADDR,,}']))" 2>/dev/null)
  [ "$n" = 0 ] && { FIN=true; break; }
  sleep 1
done
fact l1_withdrawal_finalized "$FIN"
fact l1_account_value_final "\"$(l1_account_value "$USER_ADDR")\""

# divergence check across validators (recorded; a divergence fails the e2e)
if grep -l "STATE DIVERGENCE" "$NET"/node*/node.log >/dev/null 2>&1; then fail no_state_divergence; fi
step no_state_divergence true null

run_forge; FORGE_DONE=1
finish
