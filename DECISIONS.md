# Decisions

Format: `D<n> (date) [area] decision — reasoning. Affects: crates.`
Interface changes bump `interfaces::INTERFACES_REV`.

**D1 (2026-11-10) [consensus]** Pipelined HotStuff with 2-chain commit and
timeout-certificate view change (Jolteon/HotStuff-2 style) rather than the
original 3-chain HotStuff. — Same pipelining (one QC per block piggybacked on
the next proposal), one fewer round to commit (lower block-to-finality
latency for the 1 s target), simpler and well-proven safety argument
(lock = highest QC; TC carries high QCs). Affects: consensus.

**D2 [state] Fixed-point scales**: Px u64 1e-8, Sz i64 1e-8, Usd i64 1e-6,
Rate i64 1e-12, i128 intermediates, explicit rounding everywhere. — Covers HL
price/size precision (≤ 6 px decimals for perps, sz ≤ 5 decimals) with
headroom; USDC is natively 6 decimals so bridge amounts map 1:1.

**D3 [consensus/state] Execute-after-commit with lagged root in header**
(`exec_height`, `exec_root`). — Avoids speculative execution on forks
(no state forking), keeps execution off the consensus critical path, and still
certifies every root (needed for divergence detection and snapshot sync).
Cost: the root for height h is certified a few blocks later.

**D4 [state] State commitment = sorted (key, entity-hash) list hashed with
blake3**, entity hashes recomputed only for dirty entities. — O(dirty) work
per block, trivially deterministic, enough for divergence detection and
snapshot verification. No Merkle proofs (not required by any AC).

**D5 [crypto] One secp256k1 key per validator** for consensus votes, oracle
txs, bridge attestations, and bridge withdrawal signatures; validator id =
Ethereum address. — Bridge contract needs ECDSA anyway; QCs are plain
signature lists (n ≤ 7, aggregation unnecessary).

**D6 [sdk] Example scope for AC5.** Python examples that exercise features
outside this chain's scope (vaults, sub-accounts, multisig, staking,
spot/perp deploy, builder fees, EVM, dex abstraction, referrer rewards) are
excluded; the in-scope list is in TEST_PLAN.md AC5 and covers every feature
the AC names. TS SDK `@nktkas/hyperliquid` has no examples directory; its
README examples are saved verbatim as scripts (only the transport URL set).

**D7 [perf] Throughput counts order actions**, i.e. each order, cancel, or
modify inside a signed action (HL's own accounting; MMs batch). The AC3
benchmark uses batches of 50, documented in TEST_PLAN.md.

**D8 [determinism] Replay methodology.** The week of order flow is generated
once into a block log (real signed txs, sim timestamps); the network replays
it through real consensus with leaders proposing the logged blocks, so two
runs produce identical block sequences and roots are comparable per height.

**D9 [sdk] Local chain is "testnet" for signing** (`is_mainnet=false`):
the SDKs derive mainnet-ness from the base URL, so any localhost URL signs
with source "b" / hyperliquidChain "Testnet".

**D10 [scope] Spot**: spot tokens exist for balances, spotSend and
usdClassTransfer only (USDC index 0, PURR index 1 at genesis). No spot order
books. `spotMeta` lists them so SDK constructors work.

**D11 [process] Parallel build**: each component agent works in its own git
worktree (`/home/user/wt/<name>`, branch `agent/<name>`), editing only its
directories; the lead merges to `main` after `cargo build --workspace` and
`cargo test --workspace` pass. All worktrees share
`/home/user/.cargo-target/hlbft` (via `.cargo/config.toml`) so third-party
crates compile once.

**D12 [tooling] Foundry** installed from npm (`@foundry-rs/forge` 1.7.1)
because GitHub release downloads are blocked; `forge-std` is vendored into
`bridge/lib/forge-std` from a git clone (standard library, permitted).

**D13 [api] /exchange waits for the committed receipt** (≤ 10 s) so responses
carry real statuses (resting oid / filled / error), as HL's do.

**D14 [consensus] Round-robin leaders, equal stake in test configs**;
quorum math is stake-weighted so unequal stakes work.

**D15 (rev 2) [interfaces]** Added `ApiBackend` (node<->api), `BridgeHost`
(node<->relayer watcher), `TriggerStore` (state<->book), and
`ConsensusEnv::proposal_override` (replay mode, D8). Spot balances generalized
to token indices (`AccountView.spot_balances`, `Clearinghouse::spot_send(token)`,
`Genesis.spot_tokens/spot_balances`) because the in-scope SDK example
basic_spot_transfer sends PURR.

**D16 [book]** `StopTaker` always cancels the taker remainder (doc fix, no
type change), and a partial `Fill` continues matching with the same maker
re-offered. Duplicate cloids per user must be rejected by the state crate.

**D17 (rev 3) [interfaces]** `Application::check_tx` returns the recovered
signer `Result<Address, AdmissionError>` (mempool request) so per-signer rate
limiting doesn't recover the signature twice. Node wires it via
`Pool::insert_prechecked` / `signer_of`.

**D18 [bridge]** Bridge semantics the L1 must honor: deposit ids start at 0
and are monotone; `PendingWithdrawal.amount` is the **net** amount the
destination receives (user debited amount = net + withdraw_fee); validator
actions are resubmitted by watchers, so duplicate attestations/signatures per
(id, validator) are idempotent no-ops (not errors); genesis
`bridge.contract` = deployed Bridge, `eth_chain_id` = 31337; Anvil runs with
`--block-time 1` so confirmations progress. L1-driven validator-set rotation
is out of scope for AC6 (contract rotation is tested in Foundry).

**D19 [clearinghouse]** Interface requests from clearinghouse (fee param on
withdraw, post-liquidation check, all-entity-hashes, genesis flows helper)
are served by inherent methods on the concrete `Clearinghouse`
(`withdraw_with_fee`, `negative_equity_accounts`, `all_entity_hashes`,
`genesis_total`) instead of widening the trait — the state crate binds to the
concrete type. Known gap: the insurance fund can go negative when covering
bad debt of an account with no positions left to ADL; conservation still
holds and it's reported via BadDebtCovered. Revisit if AC4 evidence shows it.

**D20 [consensus]** Votes are broadcast to all validators (n ≤ 7) so every
node forms QCs (unicast-to-next-leader stalled badly with 2/7 crashed). Two
vote domain tags (direct vs TC-extension) so commit proofs verify from QCs
alone. Lagging nodes may vote on blocks with fetched-but-unconnected
ancestry (safety depends only on QC/TC views; fixes a sync deadlock found by
the seed sweep). Replay voters also check `proposal_override`; logs need two
padding blocks. Proposals are authenticated by the p2p channel identity, not
a separate signature (validator-only mesh, D5).

**D21 [process]** Disk allowance (~30 GB) filled from incremental caches
(13 GB) in the shared target dir. Dev/test profiles now use
`incremental = false` and `debug = "line-tables-only"`. Agents must delete
their own temp dirs (e.g. /tmp perf stores) after tests.

**D22 (rev 4) [interfaces/api]** Added `StateQuery::leverage_of` (default
None; state implements it) so flat accounts show leverage after
updateLeverage. Not adopted: `BlockOutcome.timestamp_ms` (API uses
`StateQuery::block_time()` inside the query of the block), and
`RestingOrderView.is_position_tpsl/children` (cosmetic for
frontendOpenOrders; revisit only if an SDK example needs it). Transfer
ledger events: API dedupes by tx hash, either emission style works.
