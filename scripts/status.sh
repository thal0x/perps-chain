#!/usr/bin/env bash
# Runs every suite and prints pass/fail counts per suite and per acceptance
# criterion. Usage:
#   scripts/status.sh            # unit suites + AC smoke runs + evidence check
#   scripts/status.sh --quick    # unit suites + evidence check (no smoke runs)
#   scripts/status.sh --md       # also write STATUS.md
# Exit code 0 iff every suite passes and every AC is PASS.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# Authoritative builds on main use their own target dir: worktrees share
# crate names, so a shared dir can serve another worktree's stale artifacts (D24).
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/home/user/.cargo-target/main}"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
QUICK=0; MD=0
for a in "$@"; do case "$a" in --quick) QUICK=1;; --md) MD=1;; esac; done
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT
ROWS="$OUT/rows.tsv"; : > "$ROWS"

row() { printf '%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" >> "$ROWS"; }

# ---- Rust crates ----------------------------------------------------------
CRATES=$(python3 - <<'EOF'
import re
s=open("Cargo.toml").read()
m=re.search(r"members\s*=\s*\[(.*?)\]", s, re.S)
print(" ".join(x.split("/")[-1] for x in re.findall(r'"([^"]+)"', m.group(1))))
EOF
)
echo "== building workspace" >&2
if ! cargo build --workspace --all-targets >"$OUT/build.log" 2>&1; then
  row "build" "cargo build --workspace" 0 1
  tail -30 "$OUT/build.log" >&2
else
  row "build" "cargo build --workspace" 1 0
fi
for c in $CRATES; do
  echo "== cargo test -p $c" >&2
  cargo test -q -p "$c" --no-fail-fast >"$OUT/t_$c.log" 2>&1
  rc=$?
  read -r p f < <(python3 - "$OUT/t_$c.log" <<'EOF'
import re,sys
t=open(sys.argv[1],errors="replace").read()
p=sum(int(x) for x in re.findall(r"(\d+) passed",t)); f=sum(int(x) for x in re.findall(r"(\d+) failed",t))
print(p,f)
EOF
)
  if [ "$rc" -ne 0 ] && [ "$f" -eq 0 ]; then f=1; fi   # compile error counts as failure
  row "crate" "$c" "$p" "$f"
done

# ---- Solidity -------------------------------------------------------------
if [ -f bridge/foundry.toml ]; then
  echo "== forge test" >&2
  (cd bridge && forge test >"$OUT/forge.log" 2>&1); rc=$?
  read -r p f < <(python3 - "$OUT/forge.log" <<'EOF'
import re,sys
t=open(sys.argv[1],errors="replace").read()
m=re.findall(r"(\d+) tests passed, (\d+) failed",t)
print(*(m[-1] if m else ("0","1")))
EOF
)
  [ "$rc" -ne 0 ] && [ "$f" -eq 0 ] && f=1
  row "suite" "forge (bridge)" "$p" "$f"
  for req in test_ReplayProtection test_SignatureThreshold test_ValidatorSetRotation; do
    if grep -qE "\[PASS\] ${req}" "$OUT/forge.log"; then row "suite" "forge:$req*" 1 0; else row "suite" "forge:$req*" 0 1; fi
  done
else
  row "suite" "forge (bridge)" 0 1
fi

# ---- static checks --------------------------------------------------------
if scripts/check_no_float.sh >"$OUT/nofloat.log" 2>&1; then row "suite" "no-float" 1 0; else row "suite" "no-float" 0 1; cat "$OUT/nofloat.log" >&2; fi

# ---- acceptance criteria --------------------------------------------------
for n in 1 2 3 4 5 6; do
  smoke="SKIP"
  if [ "$QUICK" -eq 0 ]; then
    if [ -x scripts/acceptance.sh ]; then
      echo "== AC$n smoke" >&2
      if timeout 1800 scripts/acceptance.sh "ac$n" --smoke >"$OUT/ac${n}_smoke.log" 2>&1; then smoke="ok"; else smoke="fail"; fi
    else
      smoke="fail"
    fi
  fi
  verdict=$(python3 scripts/evidence.py "ac$n" "$smoke")
  echo "$verdict" > "$OUT/ac$n.verdict"
done

# ---- report ---------------------------------------------------------------
python3 - "$ROWS" "$OUT" "$MD" <<'EOF'
import sys,os,datetime,subprocess
rows=[l.rstrip("\n").split("\t") for l in open(sys.argv[1])]
out=sys.argv[2]; md=sys.argv[3]=="1"
lines=[]
head=subprocess.run(["git","rev-parse","--short","HEAD"],capture_output=True,text=True).stdout.strip()
dirty=subprocess.run(["git","status","--porcelain","--untracked-files=no"],capture_output=True,text=True).stdout.strip()!=""
lines.append(f"# STATUS  (commit {head}{' +dirty' if dirty else ''}, {datetime.datetime.utcnow().strftime('%Y-%m-%d %H:%M UTC')})")
lines.append("")
lines.append("| kind | suite | passed | failed | result |")
lines.append("|---|---|---:|---:|---|")
allok=True
for k,n,p,f in rows:
    ok=int(f)==0 and int(p)>0
    allok&=ok
    lines.append(f"| {k} | {n} | {p} | {f} | {'PASS' if ok else 'FAIL'} |")
lines.append("")
lines.append("| AC | criterion | smoke | evidence | result |")
lines.append("|---|---|---|---|---|")
names={1:"consensus safety 12h/7 validators",2:"determinism 50M-action week",3:"throughput 100k/s, block<1s",4:"clearinghouse invariants",5:"SDK compatibility",6:"bridge e2e + foundry"}
for n in range(1,7):
    v=open(os.path.join(out,f"ac{n}.verdict")).read().strip().split("|")
    res,smoke,ev=v[0],v[1],v[2]
    allok&=res=="PASS"
    lines.append(f"| AC{n} | {names[n]} | {smoke} | {ev} | {res} |")
print("\n".join(lines))
if md:
    open("STATUS.md","w").write("\n".join(lines)+"\n")
sys.exit(0 if allok else 1)
EOF
