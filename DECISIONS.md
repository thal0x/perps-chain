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

**D23 [test fix, AC5]** "No `status: err` in example output" was stricter
than HL itself: `basic_leverage_adjustment.py` sets ETH to 21x cross, then
21x isolated, then adds $1 isolated margin "to the ETH position". With no
ETH position, HL rejects the margin add (nothing to add margin to); with an
open position, HL rejects the cross->isolated switch. No starting state makes
every call succeed on real HL. The criterion now allows errors only via
`acceptance/sdk/expected_errors.json` (call, exact text, justification), and
the runner fails if an allowlisted error does not occur. The chain is not
special-cased for any example. Also: long-running examples (basic_ws,
basic_adding) are judged by channel/message receipt within a timeout.

**D24 [process]** Worktrees sharing one target dir can get each other's
stale artifacts (same crate names/metadata hash, mtime fingerprints). The
merge gate and status.sh on main build in a separate
`/home/user/.cargo-target/main` so verification is authoritative; agents keep
the shared dir for speed and `touch` sources if they suspect staleness.
AC4 fix (clearinghouse e81b764): non-liquidated reducing fills may not create
new negative equity; isolated opens through the mark fund the instant loss.

**D25 [AC5]** Three AC5 gaps from the first full run:
1. basic_ws subscribes to `trades` for PURR/USDC; HL trades it continuously,
   our D10 had no spot books. Resolution: **implement spot trading for
   PURR/USDC** (asset 10000, spot balance settlement, no margin) rather than
   exempting the channel. Supersedes D10's "no spot order books".
2. TS README `withdraw3({amount:"1"})` with HL's $1 fee nets zero; the chain
   rejects ("Withdrawal amount must be greater than the withdrawal fee.").
   HL's exact behaviour could not be verified offline; allowlisted in
   expected_errors.json with this justification and marked text-unverified.
   Revisit if HL's text/behaviour is learned.
3. `/info outcomeMeta` missing (TS SymbolConverter calls it) — API bug; return
   `{"outcomes":[],"questions":[]}`.
Note: chaos depends on `consensus` for offline QC verification (process-level
otherwise) — ARCHITECTURE §1 dependency note updated.

**D26 [spot]** Spot fees accrue to the spot USDC balance of reserved
account `state::SPOT_FEE_ACCOUNT` (0x…fee5) instead of the perp fee pool (no
clearinghouse entry point needed; conservation stays exact). Spot asset ids
are 10000 + pair index. Fees are charged in USDC on both sides (HL charges
the received token; not modeled). Snapshot format and roots changed with the
spot entity (no compatibility with older snapshots needed pre-launch).

**D27 [AC5 harness]** The TS SDK throws `ApiRequestError` on an `err`
response, so the verbatim README "Trading" snippet ends by throwing on the
allowlisted `withdraw3({amount:"1"})` (D25) and node exits 1 — as it would
against real HL. The TS harness treats this as pass only when the throw is the
allowlisted, matched error, raised by the snippet's final call, with no other
failure recorded; self-tests cover the negative cases. Equivalent to the
Python rule (D23), not a relaxation of it.

**D28 [process] Code freeze** at main c0dd61b, source
fingerprint `8cca0b788895:db263aa35d91:6a118c8621b7:af3525ddaf6f`. Final acceptance runs execute on this tree in order
AC5, AC6, AC2 (50M week), AC4 (reuses AC2 log), then AC1 (12 h). Any change
under crates/, bridge/, Cargo.lock or acceptance/sdk after this point
invalidates evidence (STALE) and requires reruns; only docs and runner-side
fixes outside the fingerprint are allowed. API indexer stays fed in replay
mode (real components; memory plateaus at its cap, ~1 GB/node).

**D29 [consensus] Freeze lifted** (D28) after a merge-gate proptest seed
exposed two real liveness bugs: (1) snapshot sync could never certify a
snapshot when no header's exec claim landed exactly on the snapshot height
(claims skip heights when execution lag varies; a lagging leader claims stale
heights) — fix: claims never jump over the next snapshot height, pending
snapshots are kept and re-verified; (2) pacemaker backoff carried over from a
lossy episode could delay resume past 10 s after a heal — fix: no backoff
escalation for views without a proposal, and node caps view timeout at 4 s.
All evidence will be regenerated on the new frozen tree.

**D30 [process] Re-freeze** at main 4232262 (after D29
consensus + node fixes), source fingerprint `3ffba2b27be4:db263aa35d91:6a118c8621b7:af3525ddaf6f`. Final sequence: AC3,
AC5, AC6, AC2 (50M week), AC4, AC1 (12 h). Same rules as D28.

**D31 [tooling] Portable checkout**: removed `.cargo/config.toml`, which forced
`target-dir = /home/user/.cargo-target/hlbft` and broke every build outside the
cloud box (e.g. macOS: "failed to create directory /home/user/.cargo-target").
Default is now `./target`; the box's scripts and agents already set
`CARGO_TARGET_DIR` explicitly (D24). `scripts/localnet.sh` seeding now finds the
SDK python via `HL_PYTHON`, the dev venv, or `python3`, and says how to fix it
when none has the SDK. Neither file is in the evidence fingerprint, so D30
evidence stays valid. Known remaining non-Linux gap: `crates/chaos` uses
`prctl(PR_SET_PDEATHSIG)` (Linux-only); build elsewhere with
`--workspace --exclude chaos` until it is gated behind `cfg(target_os = "linux")`,
which is deferred to the pre-AC1 refreeze because it changes the fingerprint.
