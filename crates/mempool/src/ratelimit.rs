//! Sharded token-bucket rate limiter with integer arithmetic and an injected
//! clock (milliseconds).

use parking_lot::Mutex;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hash};

#[derive(Clone, Copy, Debug)]
struct Bucket {
    /// Tokens in thousandths.
    milli: u64,
    last_ms: u64,
}

struct Shard<K> {
    map: HashMap<K, Bucket>,
    /// Size at which idle (full) buckets are purged.
    purge_at: usize,
}

/// Token bucket per key: `rate` tokens/s, capacity `burst`. `rate == 0`
/// disables the limiter (every take succeeds).
pub struct RateLimiter<K> {
    shards: Vec<Mutex<Shard<K>>>,
    hasher: std::collections::hash_map::RandomState,
    rate: u64,
    burst: u64,
}

const PURGE_MIN: usize = 4096;

impl<K: Hash + Eq + Clone> RateLimiter<K> {
    pub fn new(rate_per_s: u64, burst: u64) -> Self {
        Self {
            shards: (0..16).map(|_| Mutex::new(Shard { map: HashMap::new(), purge_at: PURGE_MIN })).collect(),
            hasher: Default::default(),
            rate: rate_per_s,
            burst: burst.max(1),
        }
    }

    pub fn enabled(&self) -> bool {
        self.rate > 0
    }

    fn refill(&self, b: &mut Bucket, now: u64) {
        let el = now.saturating_sub(b.last_ms);
        if el > 0 {
            // rate tokens/s == rate milli-tokens/ms
            b.milli = b.milli.saturating_add(el.saturating_mul(self.rate)).min(self.burst * 1000);
            b.last_ms = now;
        }
    }

    /// Take one token for `key` at time `now_ms`.
    pub fn take<Q>(&self, key: &Q, now_ms: u64) -> bool
    where
        K: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized + ToOwnedKey<K>,
    {
        if self.rate == 0 {
            return true;
        }
        let shard = &self.shards[(self.hasher.hash_one(key) % self.shards.len() as u64) as usize];
        let mut s = shard.lock();
        let ok = if let Some(b) = s.map.get_mut(key) {
            self.refill(b, now_ms);
            if b.milli >= 1000 {
                b.milli -= 1000;
                true
            } else {
                false
            }
        } else {
            s.map.insert(key.to_owned_key(), Bucket { milli: self.burst * 1000 - 1000, last_ms: now_ms });
            true
        };
        if s.map.len() >= s.purge_at {
            let full = self.burst * 1000;
            let rate = self.rate;
            s.map.retain(|_, b| {
                let el = now_ms.saturating_sub(b.last_ms);
                b.milli.saturating_add(el.saturating_mul(rate)) < full
            });
            s.purge_at = (s.map.len() * 2).max(PURGE_MIN);
        }
        ok
    }

    /// Number of tracked keys (for tests / metrics).
    pub fn tracked(&self) -> usize {
        self.shards.iter().map(|s| s.lock().map.len()).sum()
    }
}

/// Owned-key conversion so `take(&str)` can insert a `String` only on miss.
pub trait ToOwnedKey<K> {
    fn to_owned_key(&self) -> K;
}
impl ToOwnedKey<String> for str {
    fn to_owned_key(&self) -> String {
        self.to_string()
    }
}
impl<K: Clone> ToOwnedKey<K> for K {
    fn to_owned_key(&self) -> K {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_refills_at_rate_and_caps_at_burst() {
        let rl: RateLimiter<String> = RateLimiter::new(10, 5);
        let ok = (0..10).filter(|_| rl.take("a", 0)).count();
        assert_eq!(ok, 5);
        assert!(!rl.take("a", 50)); // 0.5 token
        assert!(rl.take("a", 100)); // 1 token
        assert!(!rl.take("a", 100));
        // long idle -> capped at burst
        let ok = (0..10).filter(|_| rl.take("a", 1_000_000)).count();
        assert_eq!(ok, 5);
        // independent keys
        assert!(rl.take("b", 1_000_000));
    }

    #[test]
    fn disabled_and_purge() {
        let rl: RateLimiter<u64> = RateLimiter::new(0, 1);
        assert!((0..100).all(|_| rl.take(&1u64, 0)));
        let rl: RateLimiter<u64> = RateLimiter::new(1000, 2);
        for k in 0..100_000u64 {
            rl.take(&k, 0);
        }
        // much later every bucket is full again -> purged on growth
        for k in 100_000..200_000u64 {
            rl.take(&k, 10_000);
        }
        assert!(rl.tracked() < 150_000, "tracked {}", rl.tracked());
    }
}
