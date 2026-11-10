//! Txs included in proposed-but-uncommitted blocks.
//!
//! The consensus core does not exclude txs of in-flight ancestors from new
//! proposals, and the mempool keeps txs until they commit, so with pipelining
//! (and a new leader every view) the next leader would re-propose the same
//! txs. The node records the payload hashes of every proposal it sees (or
//! makes) and filters them out of the next reap. Entries expire once the
//! committed height reaches their block height (committed txs are removed
//! from the mempool anyway; txs of abandoned blocks become eligible again).
//! Duplicates that slip through are harmless (the state machine rejects the
//! replayed nonce), so this is a best-effort optimisation.

use interfaces::*;
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap};

#[derive(Default)]
pub struct InFlight {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    by_hash: HashMap<TxHash, Height>,
    by_height: BTreeMap<Height, Vec<TxHash>>,
    committed: Height,
}

impl InFlight {
    pub fn add(&self, expire_at: Height, hashes: impl IntoIterator<Item = TxHash>) {
        let mut g = self.inner.lock();
        if expire_at <= g.committed {
            return;
        }
        let mut v = Vec::new();
        for h in hashes {
            let e = g.by_hash.entry(h).or_insert(expire_at);
            if *e < expire_at {
                *e = expire_at;
            }
            v.push(h);
        }
        g.by_height.entry(expire_at).or_default().extend(v);
    }

    /// Expiry height for a block we propose now (its height is not known to
    /// the env; pipelining keeps it within a few heights of the commit tip).
    pub fn own_expiry(&self) -> Height {
        self.inner.lock().committed + 3
    }

    pub fn contains(&self, h: &TxHash) -> bool {
        self.inner.lock().by_hash.contains_key(h)
    }

    pub fn len(&self) -> usize {
        self.inner.lock().by_hash.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Filter `f` while holding the lock once.
    pub fn retain_absent<T>(&self, items: Vec<(TxHash, T)>) -> Vec<(TxHash, T)> {
        let g = self.inner.lock();
        items.into_iter().filter(|(h, _)| !g.by_hash.contains_key(h)).collect()
    }

    pub fn set_committed(&self, h: Height) {
        let mut g = self.inner.lock();
        if h <= g.committed {
            return;
        }
        g.committed = h;
        let rest = g.by_height.split_off(&(h + 1));
        let expired = std::mem::replace(&mut g.by_height, rest);
        for (eh, hashes) in expired {
            for x in hashes {
                if g.by_hash.get(&x) == Some(&eh) {
                    g.by_hash.remove(&x);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expiry() {
        let f = InFlight::default();
        let a = Hash32([1; 32]);
        let b = Hash32([2; 32]);
        f.add(5, [a]);
        f.add(7, [b]);
        f.add(9, [a]); // seen again in a later block: extends
        f.set_committed(5);
        assert!(f.contains(&a) && f.contains(&b));
        f.set_committed(8);
        assert!(f.contains(&a) && !f.contains(&b));
        f.set_committed(9);
        assert!(f.is_empty());
        f.add(3, [b]);
        assert!(f.is_empty());
        assert_eq!(f.own_expiry(), 12);
    }
}
