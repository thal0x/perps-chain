#!/usr/bin/env bash
# Self-test of the D27 rule in acceptance/sdk/ts/_harness.mjs (uncaught allowlisted
# ApiRequestError at the end of a verbatim snippet). Runs tiny scripts that use the
# real pinned @nktkas/hyperliquid against a fake /exchange server (fake_exchange.mjs):
#   withdraw3 -> {"status":"err","response":"Withdrawal amount must be greater than the withdrawal fee."}
#   usdSend   -> {"status":"err","response":"Some other error"}
#   anything else -> {"status":"ok","response":{"type":"default"}}
# Cases (expected verdict):
#   allowlisted_last      pass  (allowlisted throw, final call, final_call_of_snippet matches)
#   not_allowlisted       fail  (uncaught throw whose text is not allowlisted)
#   allowlisted_not_last  fail  (allowlisted throw at call 1, snippet has 2 calls)
#   allowlisted_missing   fail  (allowlisted error never occurs)
#   wrong_call_count      fail  (allowlisted throw is the final recorded call, but the
#                                entry says the snippet's last call is #3: calls skipped)
# Exit 0 iff every case gets its expected verdict.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TS="$HERE/../ts"
DEPS="${HL_TS_DEPS:-/home/user/.cache/hlbft-acc-ts}"
[ -d "$DEPS/node_modules/@nktkas/hyperliquid" ] || { echo "selftest: deps missing in $DEPS (run run_ts.sh once)"; exit 2; }
W="$(mktemp -d)"
trap 'kill $SRV 2>/dev/null; rm -rf "$W"' EXIT
cp "$TS/_harness.mjs" "$HERE/fake_exchange.mjs" "$W/"
cp "$TS/package.json" "$W/"
ln -s "$DEPS/node_modules" "$W/node_modules"
PORT=$((20000 + RANDOM % 20000))
(cd "$W" && exec node fake_exchange.mjs "$PORT") >"$W/server.log" 2>&1 &
SRV=$!
for _ in $(seq 1 50); do (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && break; sleep 0.1; done

W3="Withdrawal amount must be greater than the withdrawal fee."
cat >"$W/allow.json" <<EOF
[
 {"script": "allowlisted_last", "call": "ExchangeClient.withdraw3", "error_text": "$W3", "final_call_of_snippet": 2, "justification": "selftest"},
 {"script": "allowlisted_not_last", "call": "ExchangeClient.withdraw3", "error_text": "$W3", "final_call_of_snippet": 2, "justification": "selftest"},
 {"script": "allowlisted_missing", "call": "ExchangeClient.withdraw3", "error_text": "$W3", "final_call_of_snippet": 1, "justification": "selftest"},
 {"script": "wrong_call_count", "call": "ExchangeClient.withdraw3", "error_text": "$W3", "final_call_of_snippet": 3, "justification": "selftest"}
]
EOF
head='import { EXAMPLE_ADDRESS, EXAMPLE_KEY } from "./_harness.mjs";
import { ExchangeClient, HttpTransport } from "@nktkas/hyperliquid";
import { privateKeyToAccount } from "viem/accounts";
const exchange = new ExchangeClient({ transport: new HttpTransport({ apiUrl: "http://127.0.0.1:'"$PORT"'", isTestnet: true }), wallet: privateKeyToAccount(EXAMPLE_KEY) });'
lev='await exchange.updateLeverage({ asset: 0, isCross: true, leverage: 5 });'
wd='await exchange.withdraw3({ destination: EXAMPLE_ADDRESS, amount: "1" });'
us='await exchange.usdSend({ destination: EXAMPLE_ADDRESS, amount: "1" });'
printf '%s\n%s\n%s\n' "$head" "$lev" "$wd" >"$W/allowlisted_last.ts"
printf '%s\n%s\n%s\n' "$head" "$lev" "$us" >"$W/not_allowlisted.ts"
printf '%s\n%s\n%s\n' "$head" "$wd" "$lev" >"$W/allowlisted_not_last.ts"
printf '%s\n%s\n' "$head" "$lev" >"$W/allowlisted_missing.ts"
printf '%s\n%s\n%s\n' "$head" "$lev" "$wd" >"$W/wrong_call_count.ts"

export HL_ACC_KEY=0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d
export HL_ACC_ADDR=0x70997970C51812dc3A010C7d01E50aec4b64a79C
export HL_ACC_ALLOWLIST="$W/allow.json" NODE_NO_WARNINGS=1
fails=0
for c in allowlisted_last:pass not_allowlisted:fail allowlisted_not_last:fail allowlisted_missing:fail wrong_call_count:fail; do
  n="${c%%:*}"; want="${c##*:}"
  (cd "$W" && HL_ACC_SCRIPT="$n" timeout 60 node "$n.ts") >"$W/$n.log" 2>&1
  rc=$?
  verdict=$(python3 - "$W/$n.log" "$rc" <<'EOF'
import json, sys
res = None
for l in open(sys.argv[1], errors="replace"):
    if l.startswith("ACC_RESULT "):
        res = json.loads(l[len("ACC_RESULT "):])
ok = res is not None and res["pass"] and sys.argv[2] == "0"
print(("pass" if ok else "fail") + "|" + "; ".join(res["reasons"] if res else ["no ACC_RESULT"]))
EOF
)
  got="${verdict%%|*}"
  if [ "$got" = "$want" ]; then echo "ok    $n: $got (rc $rc) ${verdict#*|}"; else echo "WRONG $n: got $got want $want (rc $rc) ${verdict#*|}"; tail -5 "$W/$n.log"; fails=$((fails + 1)); fi
done
[ "$fails" = 0 ] && echo "ts harness selftest: all cases as expected" || echo "ts harness selftest: $fails wrong"
exit "$fails"
