//! Runtime (non-consensus) configuration of a [`crate::Chain`].

use interfaces::*;
use std::sync::Arc;

/// EIP-712 digest of a pending withdrawal, as signed by validators and
/// checked by `Bridge.sol`. The default is [`bridge_withdrawal_digest`], a
/// port of `relayer::withdrawal_digest` (equality is tested); the node may
/// inject the relayer function directly.
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
        Self { withdrawal_digest: Arc::new(bridge_withdrawal_digest), recover: Arc::new(crate::crypto::ecrecover) }
    }
}

fn word_u64(v: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&v.to_be_bytes());
    w
}
fn word_u128(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}
fn word_address(a: &Address) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(&a.0);
    w
}

/// EIP-712 `Withdrawal(uint64 id,address destination,uint256 amount)` digest
/// under domain `EIP712Domain(string name,string version,uint256 chainId,address
/// verifyingContract)` = ("HlbftBridge", "1", eth_chain_id, contract), exactly
/// as `bridge/src/Bridge.sol` and `relayer::withdrawal_digest` compute it.
/// `amount` is the net USDC amount (6 decimals == `Usd` raw units).
pub fn bridge_withdrawal_digest(w: &PendingWithdrawal, p: &BridgeParams) -> [u8; 32] {
    use crate::crypto::keccak;
    let mut d = Vec::with_capacity(160);
    d.extend_from_slice(&keccak(b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"));
    d.extend_from_slice(&keccak(b"HlbftBridge"));
    d.extend_from_slice(&keccak(b"1"));
    d.extend_from_slice(&word_u64(p.eth_chain_id));
    d.extend_from_slice(&word_address(&p.contract));
    let domain = keccak(&d);
    let mut s = Vec::with_capacity(128);
    s.extend_from_slice(&keccak(b"Withdrawal(uint64 id,address destination,uint256 amount)"));
    s.extend_from_slice(&word_u64(w.id));
    s.extend_from_slice(&word_address(&w.destination));
    s.extend_from_slice(&word_u128(w.amount.0.max(0) as u128));
    let sh = keccak(&s);
    let mut t = Vec::with_capacity(66);
    t.extend_from_slice(&[0x19, 0x01]);
    t.extend_from_slice(&domain);
    t.extend_from_slice(&sh);
    keccak(&t)
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
