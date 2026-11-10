//! State owned directly by the state crate (not by the engines): nonces,
//! agents, scheduled cancels, oracle mirror, bridge bookkeeping, counters.

use borsh::{BorshDeserialize, BorshSerialize};
use interfaces::*;
use std::collections::{BTreeMap, BTreeSet};

/// HL keeps the 100 highest nonces per signer.
pub const NONCE_SET_SIZE: usize = 100;
pub const NONCE_PAST_WINDOW_MS: u64 = 2 * 24 * 3600 * 1000;
pub const NONCE_FUTURE_WINDOW_MS: u64 = 24 * 3600 * 1000;
/// scheduleCancel must be at least this far in the future.
pub const SCHEDULE_CANCEL_MIN_DELAY_MS: u64 = 5_000;

/// Per-address metadata (signer nonces; principal-side agents, referrer,
/// scheduled cancel).
#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct UserMeta {
    /// Up to 100 highest used nonces of this address *as a signer*.
    pub nonces: BTreeSet<u64>,
    /// Unnamed agent (replaced by each new unnamed approveAgent).
    pub unnamed_agent: Option<Address>,
    /// Named agents (persist until replaced under the same name).
    pub named_agents: BTreeMap<String, Address>,
    pub scheduled_cancel: Option<u64>,
    pub referrer: Option<String>,
    /// Set once the address was seen with funds (cached existence check).
    pub known: bool,
}

impl UserMeta {
    /// HL nonce rule check (no mutation).
    pub fn check_nonce(&self, nonce: u64, now: TimestampMs) -> Result<(), String> {
        if nonce <= now.saturating_sub(NONCE_PAST_WINDOW_MS) {
            return Err(format!("Invalid nonce: nonce {nonce} is too far in the past"));
        }
        if nonce >= now.saturating_add(NONCE_FUTURE_WINDOW_MS) {
            return Err(format!("Invalid nonce: nonce {nonce} is too far in the future"));
        }
        if self.nonces.contains(&nonce) {
            return Err("Invalid nonce: duplicate nonce".to_string());
        }
        if self.nonces.len() >= NONCE_SET_SIZE {
            let min = *self.nonces.iter().next().expect("non-empty");
            if nonce <= min {
                return Err(format!("Invalid nonce: nonce {nonce} must be greater than {min}"));
            }
        }
        Ok(())
    }

    pub fn use_nonce(&mut self, nonce: u64) {
        self.nonces.insert(nonce);
        while self.nonces.len() > NONCE_SET_SIZE {
            let min = *self.nonces.iter().next().expect("non-empty");
            self.nonces.remove(&min);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nonces.is_empty()
            && self.unnamed_agent.is_none()
            && self.named_agents.is_empty()
            && self.scheduled_cancel.is_none()
            && self.referrer.is_none()
            && !self.known
    }
}

/// Bridge bookkeeping.
#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct BridgeState {
    /// Deposits already credited.
    pub processed_deposits: BTreeSet<u64>,
    /// deposit_id -> attested content -> attesting validators.
    pub attestations: BTreeMap<u64, BTreeMap<DepositAttestation, BTreeSet<Address>>>,
    /// Withdrawals created by `withdraw3`, with collected signatures.
    pub pending_withdrawals: BTreeMap<u64, PendingWithdrawal>,
    /// withdrawal id -> validators that attested its finalization.
    pub finalize_votes: BTreeMap<u64, BTreeSet<Address>>,
    pub next_withdrawal_id: u64,
}

/// Everything the state crate owns, serialized into snapshots.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Core {
    pub height: Height,
    pub time: TimestampMs,
    pub next_oid: Oid,
    pub next_tid: Tid,
    pub flows: ExternalFlows,
    pub users: BTreeMap<Address, UserMeta>,
    /// Derived from `users` (rebuilt on restore; not hashed separately).
    #[borsh(skip)]
    pub agents: BTreeMap<Address, Address>,
    /// (time, user) index of scheduled cancels (derived from `users`).
    #[borsh(skip)]
    pub scheduled: BTreeSet<(u64, Address)>,
    pub validator_nonces: BTreeMap<Address, u64>,
    /// Oracle mirror: latest submission per (asset, validator): (px, time).
    pub oracle_subs: BTreeMap<(AssetId, Address), (Px, TimestampMs)>,
    /// Current aggregated oracle price and mark price per asset.
    pub oracle_px: Vec<Px>,
    pub mark_px: Vec<Px>,
    pub bridge: BridgeState,
    /// Parent oids (normalTpsl) that still have children waiting for fill.
    pub tpsl_parents: BTreeMap<Oid, AssetId>,
}

impl Core {
    pub fn rebuild_derived(&mut self) {
        self.agents.clear();
        self.scheduled.clear();
        for (user, m) in &self.users {
            if let Some(a) = m.unnamed_agent {
                self.agents.insert(a, *user);
            }
            for a in m.named_agents.values() {
                self.agents.insert(*a, *user);
            }
            if let Some(t) = m.scheduled_cancel {
                self.scheduled.insert((t, *user));
            }
        }
    }
}

/// Global-key entity (hashed under `StateKey::Global`).
#[derive(BorshSerialize)]
pub struct GlobalView<'a> {
    pub time: TimestampMs,
    pub next_oid: Oid,
    pub next_tid: Tid,
    pub flows: &'a ExternalFlows,
    pub validator_nonces: &'a BTreeMap<Address, u64>,
    pub tpsl_parents: &'a BTreeMap<Oid, AssetId>,
}

/// Oracle-key entity.
#[derive(BorshSerialize)]
pub struct OracleView<'a> {
    pub subs: &'a BTreeMap<(AssetId, Address), (Px, TimestampMs)>,
    pub oracle_px: &'a [Px],
    pub mark_px: &'a [Px],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_rules() {
        let now = 1_700_000_000_000u64;
        let mut m = UserMeta::default();
        assert!(m.check_nonce(now, now).is_ok());
        m.use_nonce(now);
        assert_eq!(m.check_nonce(now, now).unwrap_err(), "Invalid nonce: duplicate nonce");
        assert!(m.check_nonce(now - NONCE_PAST_WINDOW_MS, now).is_err());
        assert!(m.check_nonce(now - NONCE_PAST_WINDOW_MS + 1, now).is_ok());
        assert!(m.check_nonce(now + NONCE_FUTURE_WINDOW_MS, now).is_err());
        assert!(m.check_nonce(now + NONCE_FUTURE_WINDOW_MS - 1, now).is_ok());
        // fill to 100 with now+1..now+99 => set = now..now+99
        for i in 1..100 {
            m.use_nonce(now + i);
        }
        assert_eq!(m.nonces.len(), 100);
        // smaller than min rejected; between min and max but unused accepted
        assert!(m.check_nonce(now - 5, now).is_err());
        m.use_nonce(now + 200);
        assert_eq!(m.nonces.len(), 100);
        assert!(!m.nonces.contains(&now));
        assert!(m.check_nonce(now, now).is_err()); // now below the new min
        assert!(m.check_nonce(now + 150, now).is_ok());
    }
}
