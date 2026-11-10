//! Withdrawal relayer: turns L1 pending withdrawals carrying a quorum of
//! validator signatures into `Bridge.withdraw` transactions.

use crate::contract::{withdraw_calldata, BridgeView};
use crate::eip712::{normalize_v, recover_signer, withdrawal_amount_units, withdrawal_digest};
use crate::rpc::EthRpc;
use crate::txsign::TxSender;
use anyhow::Result;
use interfaces::{Address, BridgeParams, Hash32, PendingWithdrawal, Sig65, Usd};
use secp256k1::SecretKey;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Signatures valid for `digest` from members of the bridge's current set,
/// deduplicated and sorted ascending by signer (what `Bridge.withdraw`
/// requires), plus their summed power. Signatures whose recovered signer does
/// not match the claimed validator are dropped.
pub fn select_signatures(
    digest: &[u8; 32],
    sigs: &[(Address, Sig65)],
    validators: &[Address],
    powers: &[u128],
) -> (Vec<(Address, Sig65)>, u128) {
    let power_of: BTreeMap<Address, u128> = validators.iter().copied().zip(powers.iter().copied()).collect();
    let mut chosen: BTreeMap<Address, Sig65> = BTreeMap::new();
    for (claimed, sig) in sigs {
        let Some(signer) = recover_signer(digest, sig) else { continue };
        if signer != *claimed || !power_of.contains_key(&signer) {
            continue;
        }
        let mut s = *sig;
        s.v = normalize_v(s.v).expect("validated by recover_signer");
        chosen.entry(signer).or_insert(s);
    }
    let power = chosen.keys().map(|a| power_of[a]).sum();
    (chosen.into_iter().collect(), power)
}

/// Bridge quorum: floor(2 * total / 3) + 1 (same as the contract and the L1).
pub fn quorum(total: u128) -> u128 {
    total * 2 / 3 + 1
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayOutcome {
    Submitted { tx_hash: Hash32 },
    AlreadyUsed,
    BelowThreshold { have: u128, need: u128 },
    /// Submitted recently; waiting before retrying.
    Backoff,
    Invalid(String),
    Failed(String),
}

pub struct Relayer {
    pub rpc: EthRpc,
    pub bridge: Address,
    key: SecretKey,
    /// Last attempt per withdrawal id (successful or not).
    attempts: BTreeMap<u64, Instant>,
    pub retry_after: Duration,
}

impl Relayer {
    pub fn new(rpc_url: &str, bridge: Address, key: SecretKey) -> Self {
        Self { rpc: EthRpc::new(rpc_url), bridge, key, attempts: BTreeMap::new(), retry_after: Duration::from_secs(10) }
    }

    /// Process a batch of pending withdrawals (ascending id). Never fails
    /// because of a single withdrawal; RPC failures for the batch-level
    /// reads (chain id, validator set) return Err.
    pub async fn process(&mut self, pending: &[PendingWithdrawal]) -> Result<Vec<(u64, RelayOutcome)>> {
        let mut out = Vec::new();
        if pending.is_empty() {
            return Ok(out);
        }
        let view = BridgeView::new(&self.rpc, self.bridge);
        let chain_id = self.rpc.chain_id().await?;
        let (validators, powers) = view.validators().await?;
        let need = quorum(powers.iter().sum());
        let params = BridgeParams { eth_chain_id: chain_id, contract: self.bridge, min_deposit: Usd::ZERO };
        let mut sorted: Vec<&PendingWithdrawal> = pending.iter().collect();
        sorted.sort_by_key(|w| w.id);
        for w in sorted {
            let r = match self.prepare(&view, w, &params, &validators, &powers, need).await {
                Ok(Ok(data)) => {
                    self.attempts.insert(w.id, Instant::now());
                    match self.send(data).await {
                        Ok(tx_hash) => RelayOutcome::Submitted { tx_hash },
                        Err(e) => RelayOutcome::Failed(format!("{e:#}")),
                    }
                }
                Ok(Err(outcome)) => outcome,
                Err(e) => RelayOutcome::Failed(format!("{e:#}")),
            };
            match &r {
                RelayOutcome::Submitted { tx_hash } => tracing::info!(id = w.id, %tx_hash, "withdrawal relayed"),
                RelayOutcome::Failed(e) | RelayOutcome::Invalid(e) => tracing::warn!(id = w.id, "withdrawal: {e}"),
                _ => tracing::debug!(id = w.id, ?r, "withdrawal not relayed"),
            }
            out.push((w.id, r));
        }
        let live: std::collections::BTreeSet<u64> = pending.iter().map(|w| w.id).collect();
        self.attempts.retain(|id, _| live.contains(id));
        Ok(out)
    }

    async fn send(&self, data: Vec<u8>) -> Result<Hash32> {
        let sender = TxSender::new(&self.rpc, self.key).await?;
        Ok(sender.send_and_wait(Some(&self.bridge), data).await?.tx_hash)
    }

    /// Ok(Ok(calldata)) when the withdrawal should be submitted now.
    async fn prepare(
        &self,
        view: &BridgeView<'_>,
        w: &PendingWithdrawal,
        params: &BridgeParams,
        validators: &[Address],
        powers: &[u128],
        need: u128,
    ) -> Result<std::result::Result<Vec<u8>, RelayOutcome>> {
        if w.amount.raw() <= 0 || w.destination == Address::ZERO {
            return Ok(Err(RelayOutcome::Invalid("non-positive amount or zero destination".into())));
        }
        if self.attempts.get(&w.id).is_some_and(|t| t.elapsed() < self.retry_after) {
            return Ok(Err(RelayOutcome::Backoff));
        }
        if view.used_withdrawal(w.id).await? {
            return Ok(Err(RelayOutcome::AlreadyUsed));
        }
        let digest = withdrawal_digest(w, params);
        let (chosen, have) = select_signatures(&digest, &w.signatures, validators, powers);
        if have < need {
            return Ok(Err(RelayOutcome::BelowThreshold { have, need }));
        }
        let sigs: Vec<Sig65> = chosen.into_iter().map(|(_, s)| s).collect();
        Ok(Ok(withdraw_calldata(w.id, &w.destination, withdrawal_amount_units(w), &sigs)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eip712::{parse_secret_key, secret_address, sign_digest};

    #[test]
    fn quorum_matches_l1() {
        for total in 1u64..200 {
            let vs = interfaces::ValidatorSet {
                validators: vec![interfaces::ValidatorInfo { address: Address::ZERO, stake: total, p2p_addr: String::new() }],
            };
            assert_eq!(quorum(total as u128), vs.quorum_stake() as u128);
            // strictly more than 2/3
            let q = quorum(total as u128);
            assert!(3 * q > 2 * total as u128 && 3 * (q - 1) <= 2 * total as u128);
        }
    }

    #[test]
    fn select_filters_sorts_dedups() {
        let keys: Vec<SecretKey> =
            (1..=5u8).map(|i| parse_secret_key(&hex::encode([i; 32])).unwrap()).collect();
        let addrs: Vec<Address> = keys.iter().map(secret_address).collect();
        let digest = [7u8; 32];
        // validator set = keys 0..4 (sorted), key 4 is an outsider
        let mut vals: Vec<Address> = addrs[..4].to_vec();
        vals.sort();
        let powers = vec![1u128, 2, 3, 4];
        let mut sigs: Vec<(Address, Sig65)> =
            keys.iter().zip(&addrs).map(|(k, a)| (*a, sign_digest(k, &digest))).collect();
        sigs.reverse();
        // duplicate of the first signer
        sigs.push(sigs[1]);
        // wrong claimed address
        sigs.push((addrs[0], sign_digest(&keys[1], &digest)));
        // signature over another digest
        sigs.push((addrs[2], sign_digest(&keys[2], &[8u8; 32])));
        // v in 0/1 form gets normalised
        sigs[0].1.v -= 27;
        let (chosen, power) = select_signatures(&digest, &sigs, &vals, &powers);
        assert_eq!(chosen.len(), 4);
        assert_eq!(power, 10);
        let got: Vec<Address> = chosen.iter().map(|(a, _)| *a).collect();
        assert_eq!(got, vals);
        assert!(chosen.iter().all(|(_, s)| s.v == 27 || s.v == 28));
    }
}
