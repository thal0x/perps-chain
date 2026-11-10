//! Runtime (non-consensus) configuration of a [`crate::Chain`].

use interfaces::*;
use std::sync::Arc;

/// EIP-712 digest of a pending withdrawal, as signed by validators and
/// checked by `Bridge.sol`. Provided by the bridge engineer (relayer crate);
/// the default is a placeholder (see [`placeholder_withdrawal_digest`]).
pub type WithdrawalDigestFn = Arc<dyn Fn(&PendingWithdrawal, &BridgeParams) -> [u8; 32] + Send + Sync>;
/// Recover the Ethereum address that signed a 32-byte digest.
pub type RecoverFn = Arc<dyn Fn(&[u8; 32], &Sig65) -> Option<Address> + Send + Sync>;

#[derive(Clone)]
pub struct BridgeHooks {
    pub withdrawal_digest: WithdrawalDigestFn,
    pub recover: RecoverFn,
}

impl Default for BridgeHooks {
    fn default() -> Self {
        Self { withdrawal_digest: Arc::new(placeholder_withdrawal_digest), recover: Arc::new(crate::crypto::ecrecover) }
    }
}

/// Placeholder digest until the bridge crate provides the real EIP-712 one:
/// keccak256("hlbft-withdrawal" || eth_chain_id_be8 || contract || id_be8 ||
/// destination || amount_be8).
pub fn placeholder_withdrawal_digest(w: &PendingWithdrawal, p: &BridgeParams) -> [u8; 32] {
    use sha3::{Digest, Keccak256};
    let mut h = Keccak256::new();
    h.update(b"hlbft-withdrawal");
    h.update(p.eth_chain_id.to_be_bytes());
    h.update(p.contract.0);
    h.update(w.id.to_be_bytes());
    h.update(w.destination.0);
    h.update((w.amount.0 as u64).to_be_bytes());
    h.finalize().into()
}

/// When to run the (expensive) full invariant check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvariantMode {
    Never,
    EveryBlock,
    /// Every N-th height (N > 0).
    EveryN(u64),
}

#[derive(Clone)]
pub struct ChainConfig {
    pub invariants: InvariantMode,
    /// Panic on invariant violation (replay / tests). Otherwise the violation
    /// is recorded (`Chain::invariant_violations`).
    pub panic_on_invariant_violation: bool,
    /// Number of past roots served by `root_at`.
    pub root_window: usize,
    /// Max entries in the shared signature cache (cleared when exceeded).
    pub sig_cache_capacity: usize,
    pub bridge: BridgeHooks,
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self {
            invariants: InvariantMode::EveryN(100),
            panic_on_invariant_violation: false,
            root_window: 10_000,
            sig_cache_capacity: 1_000_000,
            bridge: BridgeHooks::default(),
        }
    }
}

impl ChainConfig {
    /// Config for tests and replay: invariants every block, panic on violation.
    pub fn strict() -> Self {
        Self { invariants: InvariantMode::EveryBlock, panic_on_invariant_violation: true, ..Default::default() }
    }
}
