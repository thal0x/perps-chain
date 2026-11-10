#!/usr/bin/env bash
# Local hlbft network.
#   scripts/localnet.sh start N [--bench] [--release] [--fund-sdk-examples] [-- extra hlnode run flags]
#   scripts/localnet.sh dev          # = start 4 --fund-sdk-examples (API on 3001..3004)
#   scripts/localnet.sh stop
#   scripts/localnet.sh status
# Env: LOCALNET_DIR (default <repo>/run/localnet), BASE_P2P (26600), BASE_API (3001),
#      SEED (42), ACCOUNTS (10), WAIT_S (60), HLNODE (path to a prebuilt binary).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIR="${LOCALNET_DIR:-$ROOT/run/localnet}"
BASE_P2P="${BASE_P2P:-26600}"
BASE_API="${BASE_API:-3001}"
SEED="${SEED:-42}"
ACCOUNTS="${ACCOUNTS:-10}"
WAIT_S="${WAIT_S:-60}"
TARGET_DIR="${CARGO_TARGET_DIR:-$(cd "$ROOT" && cargo metadata --format-version 1 --no-deps 2>/dev/null | python3 -c 'import sys,json;print(json.load(sys.stdin)["target_directory"])' 2>/dev/null || echo "$ROOT/target")}"

die() { echo "localnet: $*" >&2; exit 1; }

status_json() { curl -s -m 2 "http://127.0.0.1:$1/admin/status" 2>/dev/null || true; }

height_of() {
  status_json "$1" | python3 -c 'import sys,json
try: print(json.load(sys.stdin)["committed_height"])
except Exception: print(-1)' 2>/dev/null || echo -1
}

cmd_stop() {
  [ -d "$DIR" ] || { echo "nothing to stop"; return 0; }
  local any=0
  for pf in "$DIR"/node*/hlnode.pid; do
    [ -f "$pf" ] || continue
    local pid; pid="$(cat "$pf")"
    if kill -0 "$pid" 2>/dev/null; then kill -TERM "$pid" 2>/dev/null || true; any=1; fi
  done
  if [ "$any" = 1 ]; then
    for _ in $(seq 1 50); do
      local alive=0
      for pf in "$DIR"/node*/hlnode.pid; do
        [ -f "$pf" ] && kill -0 "$(cat "$pf")" 2>/dev/null && alive=1
      done
      [ "$alive" = 0 ] && break
      sleep 0.1
    done
    for pf in "$DIR"/node*/hlnode.pid; do
      [ -f "$pf" ] && kill -0 "$(cat "$pf")" 2>/dev/null && kill -9 "$(cat "$pf")" 2>/dev/null || true
    done
  fi
  rm -f "$DIR"/node*/hlnode.pid
  echo "stopped"
}

cmd_status() {
  [ -f "$DIR/validators.json" ] || die "no localnet at $DIR"
  python3 - "$DIR" <<'EOF'
import json, sys, os, urllib.request
d = sys.argv[1]
vals = json.load(open(os.path.join(d, "validators.json")))
for v in vals:
    pidf = os.path.join(d, v["home"], "hlnode.pid")
    pid = open(pidf).read().strip() if os.path.exists(pidf) else "-"
    try:
        s = json.load(urllib.request.urlopen(f"http://{v['api_addr']}/admin/status", timeout=2))
        print(f"{v['home']} pid={pid} api={v['api_addr']} committed={s['committed_height']} executed={s['executed_height']} view={s['view']} peers={len(s['peers'])} mempool={s['mempool_len']}")
    except Exception as e:
        print(f"{v['home']} pid={pid} api={v['api_addr']} DOWN ({e.__class__.__name__})")
EOF
}

cmd_start() {
  local n="${1:-}"; shift || true
  [[ "$n" =~ ^[0-9]+$ ]] || die "usage: start N [--bench] [--release] [--fund-sdk-examples]"
  local bench="" release="" fund="" extra=()
  while [ $# -gt 0 ]; do
    case "$1" in
      --bench) bench="--bench" ;;
      --release) release="--release" ;;
      --fund-sdk-examples) fund="--fund-sdk-examples" ;;
      --) shift; extra=("$@"); break ;;
      *) die "unknown option $1" ;;
    esac
    shift
  done
  [ -n "$bench" ] && release="--release"
  local bin="${HLNODE:-}"
  if [ -z "$bin" ]; then
    (cd "$ROOT" && cargo build -q -p node --bin hlnode $release) || die "build failed"
    if [ -n "$release" ]; then bin="$TARGET_DIR/release/hlnode"; else bin="$TARGET_DIR/debug/hlnode"; fi
  fi
  [ -x "$bin" ] || die "hlnode binary not found at $bin"
  cmd_stop >/dev/null 2>&1 || true
  rm -rf "$DIR"
  mkdir -p "$DIR"
  "$bin" testnet-gen --validators "$n" --out "$DIR" --base-p2p "$BASE_P2P" --base-api "$BASE_API" \
      --accounts "$ACCOUNTS" --seed "$SEED" $fund $bench >/dev/null
  for i in $(seq 0 $((n - 1))); do
    local home="$DIR/node$i"
    nohup "$bin" run --home "$home" "${extra[@]}" >"$home/stdout.log" 2>&1 &
    echo $! >"$home/hlnode.pid"
  done
  echo "started $n nodes in $DIR (api $BASE_API..$((BASE_API + n - 1)))"
  local deadline=$(( $(date +%s) + WAIT_S ))
  while :; do
    local ok=1
    for i in $(seq 0 $((n - 1))); do
      local pid; pid="$(cat "$DIR/node$i/hlnode.pid")"
      kill -0 "$pid" 2>/dev/null || { tail -n 20 "$DIR/node$i/stdout.log" "$DIR/node$i/node.log" >&2 || true; die "node$i exited"; }
      local h; h="$(height_of $((BASE_API + i)))"
      [ "$h" -gt 2 ] 2>/dev/null || ok=0
    done
    [ "$ok" = 1 ] && break
    [ "$(date +%s)" -ge "$deadline" ] && { cmd_status || true; die "nodes did not reach height > 2 within ${WAIT_S}s"; }
    sleep 0.5
  done
  cmd_status
  [ -n "$fund" ] && python3 -c "import json;a=[x for x in json.load(open('$DIR/accounts.json')) if x.get('name')=='sdk_example'][0];print('sdk example account', a['address'], 'secret', a['secret'])"
  return 0
}

case "${1:-}" in
  start) shift; cmd_start "$@" ;;
  dev) shift; BASE_API=3001 cmd_start 4 --fund-sdk-examples "$@" ;;
  stop) cmd_stop ;;
  status) cmd_status ;;
  *) die "usage: $0 start N [--bench] [--release] | dev | stop | status" ;;
esac
