//! Stake-weighted median oracle fake.

use interfaces::*;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct SimpleOracle {
    freshness_ms: u64,
    subs: BTreeMap<(AssetId, Address), (Px, u64, TimestampMs)>,
}

impl SimpleOracle {
    pub fn new(freshness_ms: u64) -> Self {
        Self { freshness_ms, subs: BTreeMap::new() }
    }
}

impl OracleAggregator for SimpleOracle {
    fn submit(&mut self, validator: Address, stake: u64, prices: &[(AssetId, Px)], now: TimestampMs) {
        for (a, p) in prices {
            self.subs.insert((*a, validator), (*p, stake, now));
        }
    }
    fn aggregate(&self, asset: AssetId, now: TimestampMs, quorum_stake: u64) -> Option<Px> {
        let mut v: Vec<(Px, u64)> = self
            .subs
            .range((asset, Address::ZERO)..=(asset, Address([0xff; 20])))
            .filter(|(_, (_, _, t))| now.saturating_sub(*t) <= self.freshness_ms)
            .map(|(_, (p, s, _))| (*p, *s))
            .collect();
        let total: u64 = v.iter().map(|x| x.1).sum();
        if total < quorum_stake || total == 0 {
            return None;
        }
        v.sort();
        let mut acc = 0u64;
        for (p, s) in v {
            acc += s;
            if acc * 2 >= total {
                return Some(p);
            }
        }
        None
    }
}
