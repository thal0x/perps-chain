//! Oracle aggregation (`OracleState`, implements
//! `interfaces::OracleAggregator`) and off-chain validator price feeds
//! (`feeds`). Integer-only; see README.md.
//!
//! # Weighted median (normative definition)
//!
//! Given the fresh submissions `(px_i, validator_i, stake_i)` for an asset,
//! with `stake_i > 0` and total fresh stake `T = Σ stake_i`:
//!
//! 1. sort by `(px, validator address)` ascending;
//! 2. walk the sorted list accumulating stake `C_k = Σ_{i<=k} stake_i`;
//! 3. the result is `px_k` of the **first** entry with `2 * C_k >= T`.
//!
//! This is the *lower* weighted median: the smallest price such that at
//! least half of the fresh stake submitted a price `<=` it. With an even
//! split (e.g. two equal-stake validators at 100 and 200) the result is the
//! lower price (100); no averaging is ever done, so the result is always one
//! of the submitted prices. Zero-stake submissions carry no weight and are
//! ignored. The address tie-break makes the walk order total; it cannot
//! change the result value (equal prices are adjacent either way) but keeps
//! the procedure fully specified.
//!
//! **Skew resistance:** if honest validators hold strictly more than half of
//! the fresh stake, the result lies in `[min honest px, max honest px]`:
//! below the honest minimum the cumulative stake is at most the adversarial
//! stake `< T/2`; at the honest maximum it is at least the honest stake
//! `> T/2`. With `quorum_stake = floor(2·total/3)+1` and adversaries holding
//! `< 1/3` of total stake, honest stake is always `> 1/2` of the fresh stake.
//!
//! # Freshness
//!
//! A submission recorded at block time `t` is fresh at `now` iff
//! `now.saturating_sub(t) <= freshness_ms` (submissions from the "future",
//! which cannot happen with monotone block time, count as fresh).

pub mod feeds;

use borsh::{BorshDeserialize, BorshSerialize};
use interfaces::*;
use std::collections::BTreeMap;

const ENCODING_VERSION: u8 = 1;

/// Latest submission of one validator for one asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Submission {
    pub px: Px,
    /// Stake of the validator at submission time.
    pub stake: u64,
    /// Block time of the submission.
    pub time: TimestampMs,
}

/// Lower stake-weighted median, see the crate docs. `entries` is sorted in
/// place by `(px, address)`. Returns `None` if the total stake is zero.
pub fn weighted_median(entries: &mut [(Px, Address, u64)]) -> Option<Px> {
    entries.sort_unstable_by_key(|e| (e.0, e.1));
    let total: u128 = entries.iter().map(|e| e.2 as u128).sum();
    if total == 0 {
        return None;
    }
    let mut cum: u128 = 0;
    for e in entries.iter() {
        cum += e.2 as u128;
        if e.2 > 0 && 2 * cum >= total {
            return Some(e.0);
        }
    }
    unreachable!("cumulative stake reaches the total")
}

/// Errors from [`OracleState::decode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OracleDecodeError {
    Borsh(String),
    BadVersion(u8),
    NonCanonical,
}

impl std::fmt::Display for OracleDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for OracleDecodeError {}

#[derive(BorshSerialize, BorshDeserialize)]
struct OracleImage {
    version: u8,
    freshness_ms: u64,
    subs: BTreeMap<AssetId, BTreeMap<Address, Submission>>,
}

/// Oracle state: the latest submission per (asset, validator).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleState {
    freshness_ms: u64,
    /// Inner maps are never empty.
    subs: BTreeMap<AssetId, BTreeMap<Address, Submission>>,
}

impl OracleState {
    /// `freshness_ms`: genesis `params.oracle_freshness_ms`.
    pub fn new(freshness_ms: u64) -> Self {
        OracleState { freshness_ms, subs: BTreeMap::new() }
    }

    pub fn from_genesis(g: &Genesis) -> Self {
        Self::new(g.params.oracle_freshness_ms)
    }

    pub fn freshness_ms(&self) -> u64 {
        self.freshness_ms
    }

    pub fn is_fresh(&self, s: &Submission, now: TimestampMs) -> bool {
        now.saturating_sub(s.time) <= self.freshness_ms
    }

    /// All recorded submissions for `asset` (fresh or not), by address.
    pub fn submissions(&self, asset: AssetId) -> Vec<(Address, Submission)> {
        self.subs.get(&asset).map(|m| m.iter().map(|(a, s)| (*a, *s)).collect()).unwrap_or_default()
    }

    /// Fresh submissions for `asset`, by address.
    pub fn fresh_submissions(&self, asset: AssetId, now: TimestampMs) -> Vec<(Address, Submission)> {
        self.submissions(asset).into_iter().filter(|(_, s)| self.is_fresh(s, now)).collect()
    }

    /// Total stake of fresh submissions for `asset`.
    pub fn fresh_stake(&self, asset: AssetId, now: TimestampMs) -> u128 {
        self.fresh_submissions(asset, now).iter().map(|(_, s)| s.stake as u128).sum()
    }

    /// Drop stale submissions (state-changing; must be called at the same
    /// point of block execution on every node if used). Aggregation results
    /// are unaffected because stale entries are ignored anyway.
    pub fn prune(&mut self, now: TimestampMs) {
        let w = self.freshness_ms;
        for m in self.subs.values_mut() {
            m.retain(|_, s| now.saturating_sub(s.time) <= w);
        }
        self.subs.retain(|_, m| !m.is_empty());
    }

    /// Remove every submission of a validator (e.g. left the validator set).
    pub fn remove_validator(&mut self, v: &Address) {
        for m in self.subs.values_mut() {
            m.remove(v);
        }
        self.subs.retain(|_, m| !m.is_empty());
    }

    /// Canonical encoding (borsh; maps in key order).
    pub fn encode(&self) -> Vec<u8> {
        #[derive(BorshSerialize)]
        struct Ref<'a> {
            version: u8,
            freshness_ms: u64,
            subs: &'a BTreeMap<AssetId, BTreeMap<Address, Submission>>,
        }
        borsh::to_vec(&Ref { version: ENCODING_VERSION, freshness_ms: self.freshness_ms, subs: &self.subs })
            .expect("borsh to vec")
    }

    /// Inverse of [`encode`](Self::encode); rejects non-canonical bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, OracleDecodeError> {
        let img = OracleImage::try_from_slice(bytes).map_err(|e| OracleDecodeError::Borsh(e.to_string()))?;
        if img.version != ENCODING_VERSION {
            return Err(OracleDecodeError::BadVersion(img.version));
        }
        let s = OracleState { freshness_ms: img.freshness_ms, subs: img.subs };
        if s.subs.values().any(|m| m.is_empty()) || s.encode() != bytes {
            return Err(OracleDecodeError::NonCanonical);
        }
        Ok(s)
    }
}

impl OracleAggregator for OracleState {
    /// Upserts `(asset, validator) -> (px, stake, now)` for every entry; if
    /// an asset appears twice in `prices` the last entry wins.
    fn submit(&mut self, validator: Address, stake: u64, prices: &[(AssetId, Px)], now: TimestampMs) {
        for (asset, px) in prices {
            self.subs.entry(*asset).or_default().insert(validator, Submission { px: *px, stake, time: now });
        }
    }

    fn aggregate(&self, asset: AssetId, now: TimestampMs, quorum_stake: u64) -> Option<Px> {
        let m = self.subs.get(&asset)?;
        let mut entries: Vec<(Px, Address, u64)> =
            m.iter().filter(|(_, s)| self.is_fresh(s, now)).map(|(a, s)| (s.px, *a, s.stake)).collect();
        let total: u128 = entries.iter().map(|e| e.2 as u128).sum();
        if total < quorum_stake as u128 {
            return None;
        }
        weighted_median(&mut entries)
    }
}
