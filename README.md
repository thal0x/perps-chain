# hlbft

A BFT layer-1 blockchain whose state machine is a perpetual-futures exchange,
wire-compatible with Hyperliquid's `/info`, `/exchange` and WebSocket API —
the official Python SDK and `@nktkas/hyperliquid` run against it unmodified
(only the base URL changes).

* Consensus: pipelined HotStuff (2-chain commit, timeout certificates), 4–7 validators, state sync.
* Exchange: per-market price-time order books, limit/IOC/ALO/reduce-only, TP/SL triggers, modify, batching; PURR/USDC spot book.
* Clearinghouse: cross + isolated margin, leverage limits, mark price, hourly funding, liquidations, insurance fund, ADL. Deterministic fixed-point only.
* Oracle: validators submit prices, chain takes the stake-weighted median.
* Bridge: Solidity contract on Anvil (deposits, validator-multisig withdrawals, set rotation) + relayer.
* Simulator: seeded agents (market makers, takers, wash traders, liquidation hunters, oracle skewers) for load and replay.

Design: [ARCHITECTURE.md](ARCHITECTURE.md) · decisions: [DECISIONS.md](DECISIONS.md) ·
test plan: [TEST_PLAN.md](TEST_PLAN.md) · milestones: [MILESTONES.md](MILESTONES.md) ·
current status: [STATUS.md](STATUS.md) · per-crate docs: `crates/*/README.md`.

## Prerequisites

* Rust (stable, ≥ 1.80), Python 3.10+, Node 22 (for the TS SDK checks).
* Foundry (`forge`, `anvil`, `cast`). If GitHub downloads are blocked, install from npm:
  ```bash
  mkdir -p /opt/foundry && cd /opt/foundry && npm i @foundry-rs/forge @foundry-rs/anvil @foundry-rs/cast
  ```
  then put `node_modules/@foundry-rs/*-linux-amd64/bin` on `PATH`.
* For SDK runs: a venv with the official Python SDK
  (`pip install hyperliquid-python-sdk` or `pip install -e <clone>`); the AC5 runner creates/uses `/home/user/.venvs/hl` and pins the commit.

```bash
cargo build --release
```

On macOS (or any non-Linux host) leave out the fault-injection harness, which
is Linux-only for now (D31):
```bash
cargo build --release --workspace --exclude chaos
```
`scripts/localnet.sh` seeds liquidity with the Python SDK; point it at your venv
with `HL_PYTHON=/path/to/venv/bin/python` if `python3` doesn't have the SDK.

## Run a local network

```bash
scripts/localnet.sh dev
```
starts 4 validators (API on `http://127.0.0.1:3001` … `3004`, p2p 26600+),
funds the SDK example account, and seeds liquidity (perp ladders on every
market + a PURR/USDC spot ladder). Files land in `run/localnet/`:
`genesis.json`, `accounts.json` (funded keys), `sdk_config.json` (ready-made
SDK `config.json`), and `node<i>/` (logs: `node.log`, `commits.log`, `roots.log`).

```bash
scripts/localnet.sh status
```
```bash
scripts/localnet.sh stop
```
Other sizes: `scripts/localnet.sh start 7 --release`. Any node:
`hlnode run --home run/localnet/node0` (all flags: [docs/NODE_CLI.md](docs/NODE_CLI.md)).
Node status: `curl -s 127.0.0.1:3001/admin/status`.

## Point the SDKs at it

The local chain signs as **testnet** (SDKs derive mainnet-ness from the URL; D9).

**Python** (official SDK examples): copy `run/localnet/sdk_config.json` to
`examples/config.json`, then replace the base URL:
```python
from hyperliquid.info import Info
from hyperliquid.exchange import Exchange
info = Info("http://127.0.0.1:3001", skip_ws=False)
exchange = Exchange(wallet, "http://127.0.0.1:3001", account_address=address)
```
(the examples use `constants.TESTNET_API_URL`; change that one value).

**TypeScript** (`@nktkas/hyperliquid`):
```ts
const transport = new HttpTransport({ apiUrl: "http://127.0.0.1:3001", isTestnet: true });
const ws = new WebSocketTransport({ url: "ws://127.0.0.1:3001/ws", isTestnet: true });
```

## Trading UI

A web trading front end for the localnet lives in [`frontend/`](frontend/README.md):
```bash
cd frontend && npm install && npm run dev
```
then open http://localhost:5173 and pick a funded account from `run/localnet/accounts.json`.

## Run the simulator

```bash
cargo run --release -p sim -- gen --seed 42 --actions 1000000 --sim-days 1 --block-ms 6000 --validators 4 --accounts 2000 --genesis-dir run/sim -o run/sim/log.zst
```
```bash
cargo run --release -p sim -- replay run/sim/log.zst --check-invariants every --roots run/sim/roots.txt
```
Live load against a running localnet:
```bash
cargo run --release -p sim -- load --targets http://127.0.0.1:3001,http://127.0.0.1:3002 --rate 20000 --duration 60 --mode mixed --keys run/localnet/accounts.json --genesis run/localnet/genesis.json
```
Replay a log through real consensus: `hlnode run --replay-log run/sim/log.zst --genesis run/sim/genesis.json …`
(details in `crates/sim/README.md` and `acceptance/README.md`).

## Tests and acceptance

```bash
scripts/status.sh
```
runs every suite (all crates, Foundry, no-float check), the smoke version of
each acceptance criterion, and checks committed evidence; `--quick` skips the
smoke runs, `--md` rewrites STATUS.md. An AC is `PASS` only if its committed
evidence (`acceptance/results/acN.json`) meets the full thresholds **and** was
produced on the current source tree (fingerprint), and its smoke run passes.

Reproduce each acceptance criterion (full runs; each writes its evidence file
and refuses to run on a dirty tree):

| AC | command | approx. time on 4 cores |
|---|---|---|
| 1 consensus safety (7 validators, 12 h faults) | `scripts/acceptance.sh ac1` | 12 h |
| 2 determinism (50M-action week, 2 replays × 4 validators) | `scripts/acceptance.sh ac2` | ~3 h |
| 3 throughput (≥100k order actions/s, 4 validators) | `scripts/acceptance.sh ac3` | ~10 min |
| 4 clearinghouse invariants (proptests + every-block replay checks) | `scripts/acceptance.sh ac4` | reuses AC2 log |
| 5 SDK compatibility (Python examples + TS README) | `scripts/acceptance.sh ac5` | ~10 min |
| 6 bridge e2e (Anvil deposit → trade → withdraw) + Foundry | `scripts/acceptance.sh ac6` | ~5 min |

Smoke versions: append `--smoke`. Fault-injection harness directly:
`hlchaos run --validators 7 --duration 3m --max-faulty 2 --seed 1 --hlnode target/release/hlnode --workdir run/chaos`
(see `crates/chaos/README.md`). Bridge only: `cd bridge && forge test`.

## Repository layout

`crates/` (interfaces, hlwire, consensus, net, mempool, book, clearinghouse,
oracle, state, api, relayer, sim, node, chaos), `bridge/` (Foundry),
`scripts/`, `acceptance/` (runners + evidence), `docs/`.
