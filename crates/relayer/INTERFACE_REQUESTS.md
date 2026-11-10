# Interface requests (relayer / bridge)

None blocking. The current `BridgeHost`, `PendingWithdrawal`, `ValidatorAction` and `BridgeParams` are sufficient.

1. **Doc fix, `BridgeParams::min_deposit`** (genesis.rs): the comment says
   "Deposit credited once attestations carry >= this fraction of stake". The
   field is the minimum deposit in USDC (1e-6). It should match the contract's
   `minDeposit` (deploy default 5 USDC).
   ```diff
   -    /// Deposit credited once attestations carry >= this fraction of stake
   -    /// (2/3 quorum is used; field kept for documentation).
   +    /// Minimum deposit (USDC, 1e-6); must equal Bridge.sol `minDeposit`.
   +    /// Deposits are credited at 2/3 stake quorum of attestations.
        pub min_deposit: Usd,
   ```
2. **Semantics to record in DECISIONS.md.** Nothing in this list needs a type change.
   * `PendingWithdrawal.amount` is the amount the destination *receives* (net of `withdraw_fee`). It is signed as `uint256 amount` in USDC base units.
   * Bridge deposit ids start at **0**.
   * The watcher may resubmit `BridgeDeposit`, `WithdrawalSignature` and `WithdrawalFinalized` after node restarts or after a 30s timeout. The state machine must ignore duplicates for each (id, validator).
   * The contract's validator set and powers must be deployed equal to the genesis validator addresses and stakes (`VALIDATORS=… POWERS=… bridge/deploy.sh`).
3. **Future (not needed now).** If the L1 ever rotates validators, it needs a `ValidatorAction::ValidatorSetUpdateSignature { epoch, signature }` (digest: `relayer::validator_set_update_digest`). The relayer would then submit `updateValidatorSet`. The contract and the Rust calldata/digest for this already exist and are tested.
