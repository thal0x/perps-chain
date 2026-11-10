# acceptance/ — AC runners and evidence

```
scripts/acceptance.sh acN [--smoke]   # N = 1..6; full run writes acceptance/results/acN.json
```
A full run refuses to start with uncommitted changes in the fingerprinted sources
(`crates bridge Cargo.lock acceptance/sdk`), because `source_fingerprint`
(`scripts/evidence.py --fingerprint`) is taken from HEAD. `--smoke` writes
`acceptance/tmp/acN.smoke.json` and never touches committed evidence. Both build
`hlnode`/`hlrelayer` in release mode and run private copies from `run/bin/`.
AC1 delegates to `scripts/chaos_smoke.sh` (hlchaos).

## AC2 — determinism (`run_ac2`, helpers in `replay_tools.py`)
Work dir `run/ac2_<mode>` (`AC2_DIR`). `sim gen --seed 42 --actions 50M --sim-days 7
--block-ms 6000 --validators 4 --threads 2 --genesis-dir` (smoke: 200k / 1 day) → `log.zst`,
genesis.json, keys.json, gen roots (gzipped). Reused if params + source fingerprint match
(`gen.key`). Replays A and B: 4 `hlnode run --replay-log log.zst --genesis genesis.json
--invariants every --min-block-ms 1` (p2p ports from the log's genesis: 9000..9003; API
on free ports from 3301); waits for every node's `replay.json`; stall timeout
`REPLAY_STALL_S` (600 s). Keeps gzipped `roots.log` + `replay.json` per node, deletes node
homes. Offline `sim replay --check-invariants every`. Comparison per height over
A0..A3, B0..B3, offline and gen roots: any missing/different root or extra height =
mismatch; every node's `replay.json` roots_digest must equal `sim replay`'s; final height
= log length; no `STATE DIVERGENCE` lines. `phases.tsv` (wall time, peak disk, peak RSS
per node / gen / offline) goes into the evidence. Dry runs: `AC2_ACTIONS=5000000
AC2_DAYS=7 scripts/acceptance.sh ac2 --smoke`. The log is kept for AC4
(`AC2_DELETE_LOG=1` deletes it).

## AC3 — throughput (`run_ac3`, `ac3_measure.py`, `thread_cpu.py`)
`localnet.sh start 4 --bench --release` (API from 3401, p2p from 26700, 200 funded
accounts), `sim load --mode bench --rate 110000` (smoke 12000) over all 4 APIs, whole run
pre-signed before sending (`--presign-s`, 4 sign threads). Measured from node0's
commits.log (block timestamps) + roots.log (order actions): window starts 20 s after the
first block with order actions, 120 s (smoke 20 s); `min_10s_window` = min over aligned
and sliding 10 s windows; median of consecutive block timestamp deltas. Per-thread CPU of
all nodes + the load generator is sampled during the window (`thread_cpu` in the
evidence). `AC3_KEEP=1` keeps `run/ac3`.

## AC4 — invariants (`run_ac4`, `proptest_cases.py`)
`cargo test -p clearinghouse -p state`; proptest case counts come from each
`proptest!` block's `cases` (proptest prints none) and count only if the test passed.
Note: the blocks set `cases` explicitly, so `PROPTEST_CASES` has no effect. Replay part:
reuses `AC2_DIR/invariants.json` when produced with the same fingerprint and mode
(offline + 8 node replays, every block), else runs gen + replay A + offline itself.
`replay_blocks_checked` = min over offline and node checks; must equal the log length.

## AC5 — SDK compatibility (`acceptance/sdk/`)
Each language runs on its own fresh `scripts/localnet.sh dev` (4 validators, API
3001..3004, genesis-funded SDK example account, `seed` = 5-level MM ladder on
every market from funded test account #0: the fixture that gives
`basic_market_order` & co. liquidity).

**Python** (`run_python.sh`): official `hyperliquid-python-sdk` at the clone's
pinned commit (`/home/user/ext/hyperliquid-python-sdk`, venv `/home/user/.venvs/hl`).
Examples + `example_utils.py` are copied verbatim; `sed` replaces only
`constants.TESTNET_API_URL` with `"http://127.0.0.1:3001"`; the diff is printed
and checked (reversing the substitution must give the original bytes).
`config.json` = `run/localnet/sdk_config.json`. Fixed order:

basic_order, basic_order_with_cloid, basic_order_modify, cancel_open_orders,
basic_leverage_adjustment, basic_market_order, basic_tpsl, basic_schedule_cancel,
basic_agent, basic_transfer, basic_spot_transfer, basic_spot_to_perp,
basic_withdraw, basic_adding, basic_ws

(rationale in `run_python.sh`). Observation: `py_trace/sitecustomize.py` (on
PYTHONPATH) records every SDK HTTP request/response with the SDK method the
example called, and every WS subscription/message identifier (SDK's own
routing). Judge (`judge_python.py`): exit 0, no traceback; every `/exchange`
error (top-level `status: err` or a per-order `error`) must be in
`expected_errors.json` (script, call, exact text, HL justification; D23) and
every allowlisted error must occur. `basic_adding` / `basic_ws` run 40 s
(must be alive at the stop) and every subscription must receive a message
routed to it; `ws_activity.py` (fixture, same account, at +10 s) does a market
open/close and a perp<->spot transfer so user channels have events.

**TypeScript** (`run_ts.sh`): `@nktkas/hyperliquid` 0.33.3 + `viem` 2.57.8,
pinned in `ts/package.json` + `package-lock.json`, installed by `npm ci` in
`/home/user/.cache/hlbft-acc-ts` (outside the repo). Scripts in `ts/` contain the
README / docs / API-reference (`@example` JSDoc in `src/api/*/client.ts`)
snippets between `// --- BEGIN verbatim <source>` / `// --- END verbatim`;
`check_ts_verbatim.py` asserts each region equals the upstream snippet up to
exactly these substitutions:

| upstream | here |
|---|---|
| `new HttpTransport()` | `new HttpTransport({ apiUrl: "http://127.0.0.1:3001", isTestnet: true })` |
| `new WebSocketTransport()` | `new WebSocketTransport({ url: "ws://127.0.0.1:3001/ws", isTestnet: true })` |
| `privateKeyToAccount("0x...")` | `privateKeyToAccount(EXAMPLE_KEY)` (SDK example account, `accounts.json`) |
| `user: "0x..."` / `destination: "0x..."` | `EXAMPLE_ADDRESS` (usdSend: the zero address) |
| `oid: 123` / `o: 123` | `OID` (the order placed just before) |
| `from "npm:viem/accounts"` (JSDoc, Deno) | `from "viem/accounts"` |

(`hl.`-prefixed transports likewise.) `ts/_harness.mjs` (imported first) only
observes: logs every client call, fails a script on any thrown API error not in
the allowlist, checks HL's response `type` per action, counts subscription
events. D27: a snippet whose LAST SDK call is an allowlisted HL rejection throws
`ApiRequestError` uncaught; the harness exits 0 only if that error is the allowlist
entry's exact text for this script/call, the entry matched, the throwing call is the
final recorded call and its index equals the entry's `final_call_of_snippet`, and no
other failure was recorded (`selftest/ts_harness_selftest.sh`, run by `run_ts.sh`,
proves the negative cases). Order: 01_read_data, 02_trading, 04_usd_send, then 06_subscriptions
(40 s; allMids, openOrders, l2Book, userFills, orderUpdates) during which
03_order_modify_cancel and 05_market_order run.

Evidence `results/ac5.json`: `python_passed/failed`, `ts_passed/failed`, `pass`,
`source_fingerprint`, `summary`, `pins`, per-script verdicts with every error's
request/response.

## AC6 — bridge (`acceptance/bridge_e2e.sh`)
anvil `--block-time 1` :8545 → `testnet-gen` (validators) → `bridge/deploy.sh`
(VALIDATORS/POWERS = genesis validators/stakes, `DEPLOY_NAME=acceptance_e2e`,
`MINT_AMOUNT=0`) → `testnet-gen --bridge-contract` (same seed; validator set and
genesis bridge params checked) → 4 nodes with `--bridge-rpc` → `seed` →
`hlrelayer relay` (dev account 9) → user = SDK example account: 1 ETH + 1000
MockUSDC minted, `cast send` approve + deposit 1000 → L1 accountValue +1000
exactly → Python SDK IoC buy 0.01 ETH fills against the MM ask →
`withdraw_from_bridge(500)` → pendingWithdrawals amount 499 → relayer
`withdrawSubmitted` → Anvil USDC +499.000000 exactly; then `forge test`.
Evidence `results/ac6.json`: `e2e_pass`, `forge_passed/failed`, required
families, tx hashes, balances, steps. Run dir `run/bridge_e2e` is deleted after
the run (`KEEP_RUN=1` keeps it).
