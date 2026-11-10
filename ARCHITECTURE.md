# hlbft — Architecture

A BFT layer-1 whose state machine is a perpetual-futures exchange, wire
compatible with Hyperliquid's `/info`, `/exchange` and WebSocket APIs.

## 1. Crate layout

```
crates/
  interfaces/    shared types + traits at every boundary (lead-owned)
  hlwire/        HL wire format: JSON<->msgpack, action decoding, L1 + EIP-712 signing, recovery
  consensus/     sans-IO pipelined HotStuff core (2-chain commit, TC view change), file BlockStore, state sync
  net/           TCP p2p transport: authenticated full mesh, framing, gossip fan-out, chaos shim
  mempool/       tx admission, dedup, per-IP/per-signer rate limits, reaping, gossip batching
  book/          per-market order book, price-time priority matching (no margin logic)
  clearinghouse/ accounts, cross/isolated margin, fees, funding, liquidation, insurance fund, ADL
  oracle/        stake-weighted median aggregation + off-chain synthetic price feeds
  state/         the replicated state machine: executes blocks by driving book+clearinghouse+oracle+bridge, nonces, agents, state root
  api/           axum HTTP /info, /exchange, /ws; event indexer (fills, order history, candles)
  relayer/       bridge relayer + per-validator bridge watcher (Anvil JSON-RPC)
  sim/           seeded load generator + agents; block-log writer; offline replay
  node/          `hlnode` binary: wires everything; config, genesis, admin endpoints
  chaos/         fault-injection harness + safety checker (`hlchaos` binary)
bridge/          Foundry project: Bridge.sol, MockUSDC.sol, tests
scripts/         status.sh, localnet.sh, acceptance runners
acceptance/      acceptance harness inputs + committed result evidence (results/*.json)
```

Dependency direction (no cycles):
`interfaces <- {hlwire, book, clearinghouse, oracle, consensus, net, mempool}`;
`state <- {hlwire, book, clearinghouse, oracle}`; `api <- {state(via traits), hlwire}`;
`node <- everything`; `sim <- {hlwire, state}`; `chaos <- (process-level only)`.

## 2. Data flow

```
 SDK ──HTTP/WS──> api ──insert──> mempool ──gossip──> peers' mempools
                                   │ reap
                                   v
                  consensus (leader proposes Block{header, txs})
                     │  votes / QCs / timeouts over net
                     v  Commit(CommittedBlock)   (strict height order)
                  executor thread: state.execute_block(ctx, txs)
                     │  BlockOutcome{root, receipts, events}
           ┌─────────┼──────────────────┐
           v         v                  v
   consensus      api indexer +        mempool.remove_committed
   (Executed{h,   /exchange waiters +
    root})        WS fan-out
```

Validators additionally run, off-chain, an **oracle feeder** (PriceFeed ->
`ValidatorTx::OraclePrices` every 1 s) and a **bridge watcher** (Anvil logs
-> `BridgeDeposit` attestations; pending withdrawals -> `WithdrawalSignature`).

## 3. Consensus (crates/consensus)

Pipelined HotStuff with a 2-chain commit rule (the Jolteon / "HotStuff-2"
variant of chained HotStuff), n = 3f+1 validators (4–7), stake-weighted
quorums (`ValidatorSet::quorum_stake`).

* **Views & leaders**: leader(v) = validators[v mod n]. A view ends when the
  validator sees a QC or TC for it.
* **Proposal** in view v: block extends `high_qc` block; header.justify =
  high_qc; if high_qc.view != v-1 the proposal carries TC(v-1).
* **Voting rule** (safety): vote for B in view v iff v > last_voted_view and
  either B.justify.view == v-1, or TC(v-1) is attached and
  B.justify.view >= max(high_qc_view in TC). Additionally B must extend a
  known parent, timestamps must be monotone and not > now+2s, the payload hash
  must match, and the exec claim must not contradict the local root.
  `SafetyState` is fsynced before the vote is sent.
* **Votes** go to leader(v+1), which forms QC(B) and proposes in v+1
  (pipelining: each block's QC rides in the next proposal).
* **Commit rule**: B is committed when QC(B') exists for B' with
  B'.parent == B and B'.view == B.view + 1. Committing B commits all its
  uncommitted ancestors (output in height order).
* **Pacemaker**: view timeout base 1000 ms, ×1.5 per consecutive timeout, cap
  10 s; reset on progress. On timeout broadcast `TimeoutMsg(v, high_qc)`;
  2f+1 -> TC(v), enter v+1. Nodes forward the highest QC/TC they have to a peer
  that is behind (view sync).
* **Block cadence**: leader proposes as soon as it holds QC(v-1) **and**
  `min_block_interval` (default 200 ms) has elapsed since the parent
  timestamp, so blocks batch mempool txs. Empty blocks are allowed (heartbeat).
* **State sync**: `Status` beacons every 1 s. A node whose committed height
  is behind a peer's requests `SyncRequest{from,to}` (≤ 256 blocks per
  response); each `CommittedBlock` carries the commit proof (QC on block +
  QC on its child) and is verified before execution. If the gap exceeds the
  peer's retained block window, it requests a snapshot; the snapshot root
  must equal an `exec_root` certified in a verified committed header.
* **Storage**: append-only files under `data/<node>/`: `blocks.log`
  (all received blocks), `committed.log` (CommittedBlock), `safety.bin`
  (atomic rename + fsync), `snapshots/<h>.snap` every 1000 heights.
* **Execution coupling**: header carries `(exec_height, exec_root)` = last
  height executed by the proposer and its root. Execution runs strictly after
  commit, so roots lag by a few blocks; a validator that has executed
  `exec_height` refuses to vote on a mismatching root and logs
  `STATE DIVERGENCE` (counted as a safety violation by the checker).

The core is **sans-IO** (`ConsensusCore::handle(input, env) -> outputs`), so
the same code runs under a deterministic in-process network simulator (unit
and property tests with partitions / drops / reordering / Byzantine
equivocation) and under real TCP in the node.

## 4. Networking and mempool (crates/net, crates/mempool)

* Full TCP mesh between validators (n ≤ 7). Lower address dials. Handshake:
  each side signs `keccak("hlbft-handshake" || chain_id || nonce_peer)` with
  its validator key; connections from non-validators are refused.
* Frame = u32 BE length + borsh(`NetMsg`), max 32 MiB. Per-peer bounded
  outbound queue (drop-oldest for `Txs`, never drop consensus messages
  except under chaos rules).
* **Chaos shim** (outbound, per destination): drop probability, delay
  distribution (uniform [min,max]), reordering (random extra delay), and
  partitions (deny-set). Controlled at runtime via the node admin endpoint
  `POST /admin/chaos`. Used by the fault-injection harness only.
* Mempool: dedup by tx hash, `Application::check_tx` before admission,
  token-bucket rate limit per API client IP (default 2000 tx/s, burst 4000)
  and per signer (default 1000 tx/s), global cap (200k txs / 512 MiB).
  Locally-admitted txs are gossiped in batches every 10 ms; peer-gossiped
  txs are not re-gossiped (full mesh). Validator txs are reaped first.
  Committed txs are removed; txs whose nonce became invalid are evicted.

## 5. State machine (crates/state)

### 5.1 State
```
Chain {
  height, time, next_oid, next_tid,
  clearinghouse: Clearinghouse     // accounts, positions, margins, insurance, fee pool, funding
  books: Vec<Book>                 // one per market (resting orders)
  triggers: per-market trigger orders (waiting TP/SL)
  oracle: OracleState              // latest submission per validator, aggregated px
  users: BTreeMap<Address, UserMeta>  // nonce set (100 highest), agents, scheduled cancel
  agents: BTreeMap<Address /*agent*/, Address /*principal*/>
  bridge: processed deposit ids + attestations, pending withdrawals + sigs
  flows: ExternalFlows
}
```
All maps are `BTreeMap` (or index-ordered); iteration order never depends on
hashing. No floating point, no wall clock, no randomness, no threads whose
interleaving affects results.

### 5.2 Block execution (`Application::execute_block`)
1. **Begin**: set `time = ctx.timestamp_ms`. Fire due scheduled cancels.
   Settle funding if an hourly boundary was crossed (`on_block_funding`).
2. **Signature pre-pass**: recover signers of all user txs in parallel
   (rayon). Pure functions -> order-independent; results then consumed
   sequentially. (Recovered results cached by tx hash from mempool admission.)
3. **Transactions in payload order**:
   * Validator txs: verify signer ∈ validator set and nonce, apply
     (oracle submission / deposit attestation / withdrawal signature).
     Deposits are credited when attestations for the same `deposit_id` and
     identical content reach quorum stake.
   * User txs: decode action; resolve agent -> principal; HL nonce rule
     (nonce not used, > min of the 100 highest stored nonces, and within
     (time − 2 days, time + 1 day)); `expires_after` check; execute; receipt.
     Invalid signature / nonce -> error receipt, no state change.
4. **End**: aggregate oracle (stake-weighted median of fresh submissions;
   unchanged if < quorum stake fresh) -> mark price = median(oracle,
   best_bid, best_ask) (oracle if one-sided) -> `set_prices` -> premium
   sample -> activate trigger orders whose condition holds against mark
   (in (asset, oid) order) -> liquidation pass: for each target in
   address order place IOC liquidation orders through the book (bounded by
   `liquidation_slippage`), then `backstop_and_adl` for the remainder ->
   invariants (always in replay/debug; sampled every 100 blocks in
   production) -> state root.

### 5.3 Matching and margin
`book` does pure price-time matching and calls back `MatchHook::on_match`
for every candidate fill. The state crate's hook: self-trade prevention
(cancel maker, `SelfTradeCanceled`); reduce-only enforcement for both
sides; maker margin re-check (cancel maker, `MarginCanceled`); taker margin
check (`StopTaker`); then `clearinghouse.apply_fill`. Order admission checks
HL tick/lot rules (`MarketSpec::is_valid_px/sz`), min notional $10 (HL
"Order must have minimum value of $10."), ALO-crossing rejection, and
initial margin for the new order. Market orders are IOC at ±slippage, as
the SDKs already send them.

### 5.4 Clearinghouse model
Cash-settled accounting: account `balance` (cross collateral, USD), per
position `szi`, `entry_px`, isolated `margin`; realized PnL and fees move
cash at fill time; funding moves cash at settlement. Unrealized PnL =
szi × (mark − entry). Equity(cross) = balance + Σ cross uPnL; isolated
position equity = margin + uPnL. Initial margin = |ntl| / leverage;
maintenance = |ntl| × 1/(2·maxLeverage). Liquidation when equity <
maintenance. Bad debt -> insurance fund -> ADL (rank opposite-side positions
by uPnL% × leverage, close at bankruptcy price). Fees -> fee pool. Rounding
always favors the system (payer rounds up, receiver rounds down); dust goes
to the insurance fund and is accounted, so conservation is **exact**.

### 5.5 State commitment
Every component reports dirty entities as `(StateKey, entity_hash)` where
`entity_hash = blake3(canonical borsh encoding of that entity)`. The state
crate keeps `BTreeMap<StateKey, Hash32>` and computes
`root = blake3(height || Σ_sorted (borsh(key) || hash))` each block. Only dirty
entities are re-encoded, so cost is O(dirty bytes + #entities × 64 B).
Snapshot = zstd(borsh(full state)); `restore` rebuilds all entity hashes and
recomputes the root.

### 5.6 Signing (crates/hlwire), must match the SDKs bit-for-bit
* L1 actions (order, cancel, modify, updateLeverage, …):
  `connectionId = keccak(msgpack(action) || nonce_be8 || (0x00 | 0x01||vault20) || [0x00||expires_be8])`;
  EIP-712 domain `{name:"Exchange", version:"1", chainId:1337, verifyingContract:0x0}`,
  type `Agent{string source, bytes32 connectionId}`, source "a" mainnet / "b" testnet.
* User-signed actions (usdSend, spotSend, withdraw3, usdClassTransfer,
  approveAgent): EIP-712 domain `{name:"HyperliquidSignTransaction",
  version:"1", chainId:int(signatureChainId), verifyingContract:0x0}`, primary
  type `HyperliquidTransaction:<Name>` with the SDK's field lists.
* msgpack must reproduce Python `msgpack.packb` / `@std/msgpack` output for the
  same JSON (insertion-order maps, smallest int encodings, str8/16/32, floats
  as float64). The API converts incoming JSON (parsed with key order
  preserved) to msgpack once; that byte string is what goes on chain.
* Local networks are "testnet" from the SDKs' perspective (`is_mainnet`
  derives from the URL) -> `Genesis.is_mainnet = false`.

## 6. API (crates/api)
axum on `--api-port` (default 3001): `POST /info`, `POST /exchange`, `GET /ws`
(also `/ws` upgrade on `/`). `/exchange` verifies the signature, inserts into
the mempool and waits (≤ 10 s) for the receipt of the committed block, then
returns HL-shaped JSON. `/info` serves current state from `StateQuery` and
history from the indexer (fills, historical orders, funding, candles built
from fills). WebSocket supports `subscribe`/`unsubscribe`/`post`/`ping` with
channels allMids, l2Book, trades, bbo, candle, orderUpdates, userFills,
userEvents, userFundings, notification, webData2, activeAssetCtx,
clearinghouseState. Admin: `GET /admin/status`, `POST /admin/chaos`.

## 7. Bridge (bridge/, crates/relayer)
`Bridge.sol` on Anvil holds MockUSDC (6 decimals). `deposit(amount)` pulls
tokens and emits `Deposit(id, user, amount)`. Each validator's watcher waits
`confirmations` blocks, then submits `BridgeDeposit` attestation; the chain
credits at quorum. `withdraw3` debits the user (fee 1 USDC) and creates a
`PendingWithdrawal`; validators sign the EIP-712 `Withdrawal(id, destination,
amount, nonce=id)` (domain: name "HlbftBridge", chainId 31337, contract) and
submit `WithdrawalSignature`; the relayer submits `withdraw(..., sigs)` once
signatures with ≥ 2/3 of bridge-contract stake exist. The contract checks
threshold over its own validator set (sorted unique signers), replay
protection (`usedWithdrawal[id]`), and supports validator-set rotation signed
by the current set (`updateValidatorSet(newSet, newPowers, epoch, sigs)`,
epoch strictly increasing).

## 8. Simulator (crates/sim)
Seeded (ChaCha20) agents: market makers (two-sided quotes, requote),
takers (IOC/market), trend followers, wash traders (self-crossing — stopped
by STP), liquidation hunters (push price into liquidation clusters),
oracle-skew validators (one or two validators submit skewed prices), and
bridge flows. Modes:
* `sim gen --seed S --actions N --sim-days D --block-ms B -o log.zst`:
  runs an embedded `state::Chain` for agent feedback and writes a block log
  (header ctx + signed txs) — real signatures.
* `sim replay log.zst`: offline replay, checks invariants every block, prints
  per-block roots.
* `sim load --rps R --target http://…`: live load over HTTP (and the
  binary-tx admin endpoint) for throughput tests.
Nodes accept `--replay-log log.zst`: the leader proposes the logged block
for each height (payload and timestamp), so a live multi-validator run over
real consensus reproduces the same block sequence; every validator writes
`roots.log` (height, root).

## 9. Determinism rules (enforced by review + tests)
No f32/f64 in `state`, `book`, `clearinghouse`, `oracle`, `consensus`
(checked by `scripts/check_no_float.sh` in status). No `HashMap`/`HashSet`
iteration in state paths. No wall clock / RNG in the state machine. Integer
overflow is a panic (bug), never wraparound. All rounding explicit.

## 10. Performance design (AC3 target: 100k order actions/s, 4 validators)
Order actions are counted per order/cancel/modify inside signed actions
(HL batches up to many orders per action; benchmark uses batches as market
makers do). Hot paths: parallel signature recovery (cached from admission),
book as `BTreeMap<Px, VecDeque<slot>>` + slab index by oid, dirty-entity
hashing, borsh framing, executor on its own thread pipelined with consensus.
