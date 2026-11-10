//! Shared cache of decoded actions and recovered signers, keyed by tx hash.
//!
//! Filled by mempool admission (`check_tx`) and consumed by block execution
//! so that the expensive ECDSA recovery is done once per tx. Entries are pure
//! functions of the tx bytes, so caching cannot affect determinism.

use interfaces::*;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

pub type Recovered = Result<(Action, Address), String>;

#[derive(Default)]
pub struct SigCache {
    inner: Mutex<HashMap<TxHash, Arc<Recovered>>>,
    capacity: usize,
}

impl SigCache {
    pub fn new(capacity: usize) -> Self {
        Self { inner: Mutex::new(HashMap::new()), capacity }
    }
    pub fn get(&self, h: &TxHash) -> Option<Arc<Recovered>> {
        self.inner.lock().get(h).cloned()
    }
    pub fn take(&self, h: &TxHash) -> Option<Arc<Recovered>> {
        self.inner.lock().remove(h)
    }
    pub fn insert(&self, h: TxHash, r: Arc<Recovered>) {
        if self.capacity == 0 {
            return;
        }
        let mut g = self.inner.lock();
        if g.len() >= self.capacity {
            g.clear();
        }
        g.insert(h, r);
    }
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
