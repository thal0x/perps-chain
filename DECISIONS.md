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
