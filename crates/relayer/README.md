# relayer — hlbft bridge watcher + withdrawal relayer

There are no alloy/ethers dependencies. The crate has its own:
* JSON-RPC client over `reqwest`;
* ABI encoder and decoder;
* EIP-155 legacy transaction signing (RLP + `secp256k1`).

## Public API

| item | purpose |
|---|---|
| `withdrawal_digest(&PendingWithdrawal, &BridgeParams) -> [u8;32]` | The EIP-712 digest that `Bridge.sol` verifies. Domain: `"HlbftBridge"`, `"1"`, `eth_chain_id`, `contract`. Type: `Withdrawal(uint64 id,address destination,uint256 amount)`. `amount` = `w.amount.raw()` (Usd 1e-6 == USDC base units). This is the amount **received**, so the fee is already deducted. Panics if amount ≤ 0. |
| `validator_set_update_digest(..)`, `eip712::domain_separator`, `sign_digest`, `recover_signer` | Helpers. `recover_signer` applies the contract's rules (low-s, v ∈ {0,1,27,28}). |
| `DepositWatcher::new(Arc<dyn BridgeHost>, WatcherConfig)` → `.spawn()` / `.run()` / `.poll_once()` | The per-validator watcher, run inside each node (described below). |
| `WatcherConfig::new(rpc_url)` | Defaults: `confirmations: 1`, `poll_interval: 500ms`, `start_block: 0`, `max_block_range: 5000`, `resubmit_after: 30s`, `params_override: None`. Without the override, the contract and chain id come from `host.genesis().bridge`. |
| `relay::Relayer::new(rpc_url, bridge, key)` → `.process(&[PendingWithdrawal])` | Submits `withdraw` for withdrawals whose valid signatures reach the contract's quorum. Used by `hlrelayer`. |
| `relay::select_signatures`, `relay::quorum` | Pure helpers. They recover each signature and drop it if the recovered signer ≠ the claimed validator or is not in the set. The rest are deduplicated and sorted ascending, and their power is summed. |
| `api::ApiClient`, `api::parse_pending_withdrawals`, `api::to_wire` | Client for the L1 `/info pendingWithdrawals` endpoint. |
| `rpc::EthRpc`, `contract::*`, `abi::*`, `txsign::*` | Ethereum plumbing. |

### Node integration (sketch)

```rust
let mut cfg = relayer::WatcherConfig::new("http://127.0.0.1:8545");
cfg.confirmations = 1;               // Anvil must run with --block-time N (it only mines on txs otherwise)
let handle = relayer::DepositWatcher::new(bridge_host.clone(), cfg).spawn();  // on a tokio runtime
```

### Watcher behaviour

The watcher does nothing when `host.validator()` is `None`. Each poll does the following:
1. **Scan** `Deposit` and `Withdrawn` logs of `genesis.bridge.contract` up to `head - confirmations`, in chunks.
2. **Deposits.** For each final deposit with `!processed_deposit(id)`, submit `ValidatorAction::BridgeDeposit(DepositAttestation{deposit_id, user, amount, eth_tx_hash})` once. If it is still unprocessed after `resubmit_after`, submit it again (this covers a dropped mempool tx). Once processed, it is forgotten.
3. **Signatures.** For each pending withdrawal not finalized on the EVM chain, and whose `signatures` lack this validator, call `host.sign_digest(withdrawal_digest(w, genesis.bridge))`. Then submit `WithdrawalSignature{withdrawal_id, signature}` once per (withdrawal, validator), with the same resubmit rule.
4. **Finalization.** For each pending withdrawal with a final `Withdrawn` event, submit `WithdrawalFinalized{withdrawal_id}` once, with the same resubmit rule.

Restart handling:
* **Node restart.** State is memory-only. The watcher rescans from `start_block`, and `processed_deposit` and `pending_withdrawals` suppress duplicates. A deposit that was attested but not yet credited is attested again, so **the L1 must treat duplicate validator actions idempotently**. It must also do so in general, because of the resubmit rule.
* **Anvil restart or reorg.** The hash of the last scanned block is re-checked on every poll. If it changed, or the chain is now shorter, the cursor resets to `start_block` and the chain is rescanned.
* **RPC down.** The poll returns `Err` and is retried on the next tick. Withdrawal signing still happens, because it does not need the EVM.

## `hlrelayer` binary

```
hlrelayer relay   [--api http://127.0.0.1:3001] [--rpc http://127.0.0.1:8545]
                  [--deployment bridge/deployments/local.json | --bridge 0x..]
                  [--key HEX | --dev-account N (default 9)] [--poll-ms 1000] [--once]
hlrelayer deposit --amount 1000 [--dev-account N (default 1) | --key HEX]   # approve + deposit
hlrelayer mint    --to 0x.. --amount 1000                                   # MockUSDC.mint
hlrelayer balance --address 0x..                                            # {"address","usdc","raw"}
hlrelayer info                                                              # validators, epoch, quorum, balances
```
* Env equivalents: `HL_BRIDGE_RPC`, `HL_BRIDGE_ADDRESS`, `HL_BRIDGE_DEPLOYMENT`, `HL_RELAYER_KEY`, `HL_API_URL`.
* Output is JSON on stdout, logs go to stderr, and the log level is set with `RUST_LOG`.
* `deposit` prints `{"depositId","user","amount","amountRaw","approveTx","depositTx","block"}`.
* `relay` prints `{"event":"withdrawSubmitted","id","txHash"}` per submitted withdrawal.

Each loop of `relay` does the following:
1. Fetch the pending withdrawals.
2. Read `getValidators()` from the contract.
3. For each withdrawal (ascending id), skip it if `usedWithdrawal(id)`.
4. Otherwise select the valid signatures from the current contract set. If their power is ≥ `floor(2T/3)+1`, send `withdraw(w, sigs sorted by signer)` as a signed EIP-155 tx and wait for the receipt.
5. Retry a failed attempt after 10s.

Reverts are reported with the Bridge custom error name.

## L1 API contract: `POST /info {"type":"pendingWithdrawals"}`

The api engineer should return a JSON array:

```json
[{"id": 3,
  "user": "0x…",                // L1 account that withdrew
  "destination": "0x…",         // EVM recipient
  "amount": "499",              // decimal USD string: amount the destination RECEIVES (net of the 1 USDC fee)
  "created_at": 1700000000000,  // ms
  "signatures": [{"validator": "0x…", "r": "0x…", "s": "0x…", "v": 27}]}]
```

`api::to_wire(&PendingWithdrawal)` produces exactly this shape. The parser also accepts:
* the plain serde form of `interfaces::PendingWithdrawal`, where `amount` is the raw 1e-6 integer and `signatures` is `[[addr, {r,s,v}]]`;
* `createdAt`;
* a string `id`;
* `{"pendingWithdrawals": [...]}`.

## Tests

```
cargo test -p relayer      # 17 unit + 9 integration tests
```
* Unit tests:
  * ABI calldata is byte-identical to `cast calldata` (`testdata/*.hex`), and the selectors and topics match;
  * the digest test vector (domain separator, withdrawal, valset update), produced by Bridge.sol on Anvil;
  * every field is bound into the digest;
  * sign/recover, including malleability;
  * RLP, plus the official EIP-155 example transaction;
  * the quorum formula equals `ValidatorSet::quorum_stake`;
  * signature selection (filter, dedupe, sort, v normalisation);
  * API wire parsing.
* `tests/anvil.rs`:
  * Each test spawns its own `anvil` on a random port. Contracts are built with `forge build` into a temp dir and deployed by raw signed txs. These tests are **skipped with a message if `anvil`/`forge` are not on PATH**.
  * The digest equals `hashWithdrawal`, `hashValidatorSetUpdate` and `domainSeparator` on the contract.
  * The relayer at threshold: below → submitted → `AlreadyUsed`, and a raw replay reverts `WithdrawalAlreadyUsed`.
  * Rotation through Rust calldata: old-set signatures are filtered, and epoch replay reverts.
  * The watcher attests a deposit exactly once after its confirmations, and handles node-restart rescans.
  * The resubmit timeout.
  * The watcher signs a withdrawal once and the signature recovers to the validator; the relayer submits, then the watcher reports `WithdrawalFinalized` once; a non-validator host does nothing.
  * The watcher survives an Anvil restart and redeploy (chain reset detected, only new deposits attested).
  * The RPC being down.
  * The API client against a raw HTTP server.

A manual check was also done: `hlrelayer relay --once` against a stub API, with signatures made by `cast wallet sign --no-hash`, relayed a 499 USDC withdrawal.
