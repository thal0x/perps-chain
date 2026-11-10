//! Transaction pool: admission, dedup, rate limiting, FIFO reaping
//! (validator txs first), gossip batching. See `README.md`.
//!
//! ## Signer-aware limits
//! The signer of a `UserTx` is only known after signature recovery, which is
//! the node's (expensive, ~25 µs) business. The pool therefore takes the
//! signer from one of, in order:
//! 1. the `signer` argument of [`Pool::insert_prechecked`] (the node ran
//!    `check_tx` + recovery itself, e.g. in parallel, and passes the result);
//! 2. the optional [`SignerOf`] callback (e.g. a lookup in a recovery cache
//!    the node's `check_tx` fills, keyed by tx hash), called *after* the
//!    admission check succeeded;
//! 3. otherwise the per-signer limit is skipped for that tx.
//!
//! `ValidatorTx`s use their `validator` field (verified by the checker) and
//! are exempt from rate limits and capacity rejection.
//!
//! ## Locking
//! State is split into `shards` (by tx hash), each behind a `RwLock`. The
//! admission checker and signer callback run outside every lock, so
//! `insert`/`insert_batch`/`handle_peer_txs` scale across threads.

pub mod ratelimit;

use interfaces::{
    blake3_hash, AdmissionError, Address, Hash32, Mempool, NetMsg, Network, Tx, TxHash, TxSource,
};
use parking_lot::{Mutex, RwLock};
use ratelimit::RateLimiter;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BinaryHeap, HashMap, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// Admission check (stateless validation + `Application::check_tx`).
pub type AdmissionChecker = Arc<dyn Fn(&Tx) -> Result<(), AdmissionError> + Send + Sync>;
/// Signer lookup for per-signer rate limiting of user txs (see crate docs).
pub type SignerOf = Arc<dyn Fn(&Tx) -> Option<Address> + Send + Sync>;

/// Millisecond clock (monotonic). Injected for deterministic tests.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// Monotonic clock: milliseconds since construction.
pub struct SystemClock(Instant);
impl Default for SystemClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.0.elapsed().as_millis() as u64
    }
}

/// Manually advanced clock for tests.
#[derive(Default)]
pub struct ManualClock(pub AtomicU64);
impl ManualClock {
    pub fn set(&self, ms: u64) {
        self.0.store(ms, Relaxed)
    }
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Relaxed);
    }
}
impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Relaxed)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PoolConfig {
    /// Global cap on pooled txs.
    pub max_txs: usize,
    /// Global cap on pooled bytes (borsh-encoded tx size).
    pub max_bytes: usize,
    /// Per API client IP token bucket (0 = disabled).
    pub ip_rate_per_s: u64,
    pub ip_burst: u64,
    /// Per signer token bucket (0 = disabled).
    pub signer_rate_per_s: u64,
    pub signer_burst: u64,
    /// Apply the per-signer limit to peer-gossiped txs too (default: only
    /// API-submitted txs; peers already applied it at their edge).
    pub signer_limit_peer_txs: bool,
    /// How long committed hashes are remembered to reject re-gossip.
    pub recent_committed_ttl_ms: u64,
    /// Upper bound on remembered committed hashes.
    pub recent_committed_max: usize,
    /// Pooled txs older than this are expired on `remove_committed` /
    /// `prune` (0 = never). Catches txs that can never be included.
    pub tx_ttl_ms: u64,
    /// Gossip batching period; 0 disables the background gossip thread
    /// (call [`Pool::flush_gossip`] manually).
    pub gossip_interval_ms: u64,
    pub gossip_max_batch_txs: usize,
    pub gossip_max_batch_bytes: usize,
    /// Number of lock shards (rounded up to >= 1).
    pub shards: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_txs: 200_000,
            max_bytes: 512 << 20,
            ip_rate_per_s: 2000,
            ip_burst: 4000,
            signer_rate_per_s: 1000,
            signer_burst: 2000,
            signer_limit_peer_txs: false,
            recent_committed_ttl_ms: 120_000,
            recent_committed_max: 2_000_000,
            tx_ttl_ms: 120_000,
            gossip_interval_ms: 10,
            gossip_max_batch_txs: 10_000,
            gossip_max_batch_bytes: 4 << 20,
            shards: 32,
        }
    }
}

/// External dependencies of the pool.
pub struct PoolDeps {
    pub checker: AdmissionChecker,
    pub signer_of: Option<SignerOf>,
    pub network: Option<Arc<dyn Network>>,
    pub clock: Arc<dyn Clock>,
}

impl PoolDeps {
    pub fn new(checker: AdmissionChecker) -> Self {
        Self { checker, signer_of: None, network: None, clock: Arc::new(SystemClock::default()) }
    }
    pub fn with_network(mut self, n: Arc<dyn Network>) -> Self {
        self.network = Some(n);
        self
    }
    pub fn with_signer_of(mut self, f: SignerOf) -> Self {
        self.signer_of = Some(f);
        self
    }
    pub fn with_clock(mut self, c: Arc<dyn Clock>) -> Self {
        self.clock = c;
        self
    }
}

/// Counters (monotonic) + gauges.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolStats {
    pub len: usize,
    pub bytes: usize,
    pub admitted: u64,
    pub admitted_from_peers: u64,
    pub duplicate: u64,
    pub rate_limited_ip: u64,
    pub rate_limited_signer: u64,
    pub full: u64,
    pub check_failed: u64,
    pub evicted_for_validator_txs: u64,
    pub expired: u64,
    pub invalidated: u64,
    pub removed_committed: u64,
    pub gossiped_txs: u64,
    pub gossip_batches: u64,
}

#[derive(Default)]
struct Counters {
    admitted: AtomicU64,
    admitted_from_peers: AtomicU64,
    duplicate: AtomicU64,
    rate_limited_ip: AtomicU64,
    rate_limited_signer: AtomicU64,
    full: AtomicU64,
    check_failed: AtomicU64,
    evicted: AtomicU64,
    expired: AtomicU64,
    invalidated: AtomicU64,
    removed_committed: AtomicU64,
    gossiped_txs: AtomicU64,
    gossip_batches: AtomicU64,
}

/// Result of [`Pool::handle_peer_txs`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerBatchResult {
    pub admitted: usize,
    pub duplicate: usize,
    pub rejected: usize,
}

/// Hasher for keys that are already uniformly random (blake3 hashes).
#[derive(Default)]
struct PrehashedHasher(u64);
impl Hasher for PrehashedHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        // Hash32's derived Hash writes the 32-byte array (len prefix first).
        for chunk in bytes.chunks(8) {
            let mut b = [0u8; 8];
            b[..chunk.len()].copy_from_slice(chunk);
            self.0 = self.0.rotate_left(5) ^ u64::from_le_bytes(b);
        }
    }
    fn write_usize(&mut self, _: usize) {}
}
type HMap<V> = HashMap<Hash32, V, BuildHasherDefault<PrehashedHasher>>;

struct Entry {
    tx: Arc<Tx>,
    size: usize,
    seq: u64,
    inserted_ms: u64,
    is_val: bool,
}

#[derive(Default)]
struct Shard {
    map: HMap<Entry>,
    /// seq -> hash, FIFO order of user / validator txs.
    user_order: BTreeMap<u64, Hash32>,
    val_order: BTreeMap<u64, Hash32>,
    /// Recently committed: hash -> expiry ms, with FIFO for pruning.
    recent: HMap<u64>,
    recent_fifo: VecDeque<(u64, Hash32)>,
}

impl Shard {
    fn remove(&mut self, h: &Hash32) -> Option<Entry> {
        let e = self.map.remove(h)?;
        if e.is_val {
            self.val_order.remove(&e.seq);
        } else {
            self.user_order.remove(&e.seq);
        }
        Some(e)
    }
}

/// The transaction pool. Implements [`interfaces::Mempool`].
pub struct Pool {
    cfg: PoolConfig,
    deps: PoolDeps,
    shards: Vec<RwLock<Shard>>,
    count: AtomicUsize,
    bytes: AtomicUsize,
    seq: AtomicU64,
    ip_limiter: RateLimiter<String>,
    signer_limiter: RateLimiter<Address>,
    gossip_buf: Mutex<Vec<(Arc<Tx>, usize)>>,
    c: Counters,
}

impl Pool {
    /// Create a pool. If `deps.network` is set and `gossip_interval_ms > 0`,
    /// a background thread flushes gossip every interval (it exits when the
    /// pool is dropped).
    pub fn new(cfg: PoolConfig, deps: PoolDeps) -> Arc<Pool> {
        let n = cfg.shards.max(1);
        let pool = Arc::new(Pool {
            shards: (0..n).map(|_| RwLock::new(Shard::default())).collect(),
            count: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
            seq: AtomicU64::new(1),
            ip_limiter: RateLimiter::new(cfg.ip_rate_per_s, cfg.ip_burst),
            signer_limiter: RateLimiter::new(cfg.signer_rate_per_s, cfg.signer_burst),
            gossip_buf: Mutex::new(Vec::new()),
            c: Counters::default(),
            cfg,
            deps,
        });
        if pool.deps.network.is_some() && pool.cfg.gossip_interval_ms > 0 {
            let weak: Weak<Pool> = Arc::downgrade(&pool);
            let period = Duration::from_millis(pool.cfg.gossip_interval_ms);
            std::thread::Builder::new()
                .name("mempool-gossip".into())
                .spawn(move || loop {
                    std::thread::sleep(period);
                    match weak.upgrade() {
                        Some(p) => p.flush_gossip(),
                        None => return,
                    }
                })
                .expect("spawn gossip thread");
        }
        pool
    }

    pub fn config(&self) -> &PoolConfig {
        &self.cfg
    }

    fn shard_of(&self, h: &Hash32) -> usize {
        let x = u64::from_le_bytes(h.0[24..32].try_into().unwrap());
        (x % self.shards.len() as u64) as usize
    }

    /// Admit with the full pipeline (dedup, IP limit, checker, signer limit).
    pub fn insert_tx(&self, tx: Tx, source: TxSource) -> Result<TxHash, AdmissionError> {
        self.admit(tx, &source, true, None)
    }

    /// Admit a tx whose admission check the caller already ran (e.g. in a
    /// parallel verification stage that also recovered `signer`). Dedup,
    /// rate limits and capacity still apply.
    pub fn insert_prechecked(&self, tx: Tx, source: TxSource, signer: Option<Address>) -> Result<TxHash, AdmissionError> {
        self.admit(tx, &source, false, signer)
    }

    /// Admit many txs from one source. Safe to call from many threads at
    /// once; each tx's check runs outside all locks.
    pub fn insert_batch(&self, txs: Vec<Tx>, source: TxSource) -> Vec<Result<TxHash, AdmissionError>> {
        txs.into_iter().map(|tx| self.admit(tx, &source, true, None)).collect()
    }

    /// Admit a gossip batch received from `from`. These txs are never
    /// re-gossiped (full mesh).
    pub fn handle_peer_txs(&self, from: Address, txs: Vec<Tx>) -> PeerBatchResult {
        let src = TxSource::Peer(from);
        let mut r = PeerBatchResult::default();
        for tx in txs {
            match self.admit(tx, &src, true, None) {
                Ok(_) => r.admitted += 1,
                Err(AdmissionError::Duplicate) => r.duplicate += 1,
                Err(_) => r.rejected += 1,
            }
        }
        r
    }

    fn admit(&self, tx: Tx, source: &TxSource, run_check: bool, signer: Option<Address>) -> Result<TxHash, AdmissionError> {
        let enc = borsh::to_vec(&tx).map_err(|e| AdmissionError::Malformed(e.to_string()))?;
        let hash = blake3_hash(&enc);
        let size = enc.len();
        drop(enc);
        let si = self.shard_of(&hash);
        {
            let s = self.shards[si].read();
            if s.map.contains_key(&hash) || s.recent.contains_key(&hash) {
                self.c.duplicate.fetch_add(1, Relaxed);
                return Err(AdmissionError::Duplicate);
            }
        }
        let now = self.deps.clock.now_ms();
        if let TxSource::Api { ip } = source {
            if !self.ip_limiter.take(ip.as_str(), now) {
                self.c.rate_limited_ip.fetch_add(1, Relaxed);
                return Err(AdmissionError::RateLimited);
            }
        }
        let is_val = matches!(tx, Tx::Validator(_));
        if !is_val
            && (self.count.load(Relaxed) >= self.cfg.max_txs || self.bytes.load(Relaxed) + size > self.cfg.max_bytes)
        {
            self.c.full.fetch_add(1, Relaxed);
            return Err(AdmissionError::MempoolFull);
        }
        if run_check {
            if let Err(e) = (self.deps.checker)(&tx) {
                self.c.check_failed.fetch_add(1, Relaxed);
                return Err(e);
            }
        }
        let limit_signer = match source {
            TxSource::Api { .. } => true,
            TxSource::Peer(_) => self.cfg.signer_limit_peer_txs,
            TxSource::Local => false,
        };
        if !is_val && limit_signer && self.signer_limiter.enabled() {
            let signer = signer.or_else(|| self.deps.signer_of.as_ref().and_then(|f| f(&tx)));
            if let Some(s) = signer {
                if !self.signer_limiter.take(&s, now) {
                    self.c.rate_limited_signer.fetch_add(1, Relaxed);
                    return Err(AdmissionError::RateLimited);
                }
            }
        }

        // Reserve capacity.
        let c = self.count.fetch_add(1, Relaxed) + 1;
        let b = self.bytes.fetch_add(size, Relaxed) + size;
        if c > self.cfg.max_txs || b > self.cfg.max_bytes {
            if is_val {
                self.evict_oldest_user_until_fits();
            } else {
                self.count.fetch_sub(1, Relaxed);
                self.bytes.fetch_sub(size, Relaxed);
                self.c.full.fetch_add(1, Relaxed);
                return Err(AdmissionError::MempoolFull);
            }
        }

        let tx = Arc::new(tx);
        {
            let mut s = self.shards[si].write();
            if s.map.contains_key(&hash) || s.recent.contains_key(&hash) {
                drop(s);
                self.count.fetch_sub(1, Relaxed);
                self.bytes.fetch_sub(size, Relaxed);
                self.c.duplicate.fetch_add(1, Relaxed);
                return Err(AdmissionError::Duplicate);
            }
            let seq = self.seq.fetch_add(1, Relaxed);
            if is_val {
                s.val_order.insert(seq, hash);
            } else {
                s.user_order.insert(seq, hash);
            }
            s.map.insert(hash, Entry { tx: tx.clone(), size, seq, inserted_ms: now, is_val });
        }
        self.c.admitted.fetch_add(1, Relaxed);
        match source {
            TxSource::Peer(_) => {
                self.c.admitted_from_peers.fetch_add(1, Relaxed);
            }
            TxSource::Api { .. } | TxSource::Local => {
                if self.deps.network.is_some() {
                    self.gossip_buf.lock().push((tx, size));
                }
            }
        }
        Ok(hash)
    }

    /// Evict the oldest user txs until the pool is within its caps (used to
    /// make room for validator txs).
    fn evict_oldest_user_until_fits(&self) {
        while self.count.load(Relaxed) > self.cfg.max_txs || self.bytes.load(Relaxed) > self.cfg.max_bytes {
            // Find the shard with the globally oldest user tx.
            let mut best: Option<(u64, usize)> = None;
            for (i, sh) in self.shards.iter().enumerate() {
                if let Some((&seq, _)) = sh.read().user_order.first_key_value() {
                    if best.map(|(b, _)| seq < b).unwrap_or(true) {
                        best = Some((seq, i));
                    }
                }
            }
            let Some((seq, i)) = best else { return };
            let mut s = self.shards[i].write();
            let Some(h) = s.user_order.get(&seq).copied() else { continue };
            if let Some(e) = s.remove(&h) {
                self.count.fetch_sub(1, Relaxed);
                self.bytes.fetch_sub(e.size, Relaxed);
                self.c.evicted.fetch_add(1, Relaxed);
            }
        }
    }

    /// Up to `max_txs` / `max_bytes` in FIFO order, validator txs first,
    /// without removing. A tx that does not fit the remaining byte budget is
    /// skipped (smaller later txs may still fit).
    pub fn reap_txs(&self, max_txs: usize, max_bytes: usize) -> Vec<Tx> {
        let guards: Vec<_> = self.shards.iter().map(|s| s.read()).collect();
        let mut out = Vec::with_capacity(max_txs.min(self.count.load(Relaxed)));
        let mut bytes_left = max_bytes;
        for validators in [true, false] {
            if out.len() >= max_txs {
                break;
            }
            let mut iters: Vec<_> = guards
                .iter()
                .map(|g| if validators { g.val_order.iter() } else { g.user_order.iter() })
                .collect();
            let mut heap = BinaryHeap::with_capacity(iters.len());
            for (i, it) in iters.iter_mut().enumerate() {
                if let Some((&seq, h)) = it.next() {
                    heap.push(std::cmp::Reverse((seq, i, *h)));
                }
            }
            let mut consecutive_skips = 0;
            while let Some(std::cmp::Reverse((_, i, h))) = heap.pop() {
                let e = &guards[i].map[&h];
                if e.size <= bytes_left {
                    bytes_left -= e.size;
                    out.push((*e.tx).clone());
                    consecutive_skips = 0;
                    if out.len() >= max_txs {
                        break;
                    }
                } else {
                    consecutive_skips += 1;
                    if consecutive_skips > 64 {
                        break;
                    }
                }
                if let Some((&seq, h)) = iters[i].next() {
                    heap.push(std::cmp::Reverse((seq, i, *h)));
                }
            }
        }
        out
    }

    /// Remove committed txs and remember their hashes (so re-gossip of them
    /// is rejected as `Duplicate`), then expire stale txs.
    pub fn remove_committed_txs(&self, hashes: &[TxHash]) {
        let now = self.deps.clock.now_ms();
        let expiry = now.saturating_add(self.cfg.recent_committed_ttl_ms);
        let per_shard_max = (self.cfg.recent_committed_max / self.shards.len()).max(1);
        let mut by_shard: Vec<Vec<&TxHash>> = vec![Vec::new(); self.shards.len()];
        for h in hashes {
            by_shard[self.shard_of(h)].push(h);
        }
        let mut removed = 0u64;
        for (i, hs) in by_shard.into_iter().enumerate() {
            let mut s = self.shards[i].write();
            for h in hs {
                if let Some(e) = s.remove(h) {
                    self.count.fetch_sub(1, Relaxed);
                    self.bytes.fetch_sub(e.size, Relaxed);
                    removed += 1;
                }
                if s.recent.insert(*h, expiry).is_none() {
                    s.recent_fifo.push_back((expiry, *h));
                }
            }
            Self::prune_recent(&mut s, now, per_shard_max);
        }
        self.c.removed_committed.fetch_add(removed, Relaxed);
        self.prune();
    }

    fn prune_recent(s: &mut Shard, now: u64, max: usize) {
        while let Some(&(exp, h)) = s.recent_fifo.front() {
            if exp <= now || s.recent_fifo.len() > max {
                s.recent_fifo.pop_front();
                // Only drop the map entry if it was not refreshed later.
                if s.recent.get(&h) == Some(&exp) {
                    s.recent.remove(&h);
                }
            } else {
                break;
            }
        }
    }

    /// Expire txs older than `tx_ttl_ms` and committed-hash memory past its
    /// TTL. Called by `remove_committed`; may also be called periodically.
    pub fn prune(&self) {
        let now = self.deps.clock.now_ms();
        let per_shard_max = (self.cfg.recent_committed_max / self.shards.len()).max(1);
        let ttl = self.cfg.tx_ttl_ms;
        let mut expired = 0u64;
        for sh in &self.shards {
            let mut s = sh.write();
            Self::prune_recent(&mut s, now, per_shard_max);
            if ttl == 0 {
                continue;
            }
            for validators in [true, false] {
                loop {
                    let order = if validators { &s.val_order } else { &s.user_order };
                    let Some((_, h)) = order.first_key_value() else { break };
                    let h = *h;
                    if s.map[&h].inserted_ms.saturating_add(ttl) > now {
                        break;
                    }
                    let e = s.remove(&h).expect("present");
                    self.count.fetch_sub(1, Relaxed);
                    self.bytes.fetch_sub(e.size, Relaxed);
                    expired += 1;
                }
            }
        }
        self.c.expired.fetch_add(expired, Relaxed);
    }

    /// Re-run the admission checker on every pooled tx (outside locks) and
    /// evict those that fail now (e.g. nonce invalidated by a committed
    /// block). Returns the number evicted. Cost: one check per pooled tx.
    pub fn recheck(&self) -> usize {
        self.retain(|tx| (self.deps.checker)(tx).is_ok())
    }

    /// Keep only txs for which `keep` returns true (`keep` runs outside the
    /// shard locks). Returns the number evicted.
    pub fn retain(&self, keep: impl Fn(&Tx) -> bool) -> usize {
        let mut evicted = 0;
        for sh in &self.shards {
            let snapshot: Vec<(Hash32, Arc<Tx>)> = sh.read().map.iter().map(|(h, e)| (*h, e.tx.clone())).collect();
            let bad: Vec<Hash32> = snapshot.into_iter().filter(|(_, tx)| !keep(tx)).map(|(h, _)| h).collect();
            if bad.is_empty() {
                continue;
            }
            let mut s = sh.write();
            for h in bad {
                if let Some(e) = s.remove(&h) {
                    self.count.fetch_sub(1, Relaxed);
                    self.bytes.fetch_sub(e.size, Relaxed);
                    evicted += 1;
                }
            }
        }
        self.c.invalidated.fetch_add(evicted as u64, Relaxed);
        evicted
    }

    pub fn contains(&self, h: &TxHash) -> bool {
        self.shards[self.shard_of(h)].read().map.contains_key(h)
    }

    /// Broadcast buffered locally-admitted txs as `NetMsg::Txs` batches.
    pub fn flush_gossip(&self) {
        let Some(net) = &self.deps.network else { return };
        let buf = std::mem::take(&mut *self.gossip_buf.lock());
        if buf.is_empty() {
            return;
        }
        let mut batch = Vec::new();
        let mut bytes = 0;
        let send = |batch: &mut Vec<Tx>| {
            self.c.gossiped_txs.fetch_add(batch.len() as u64, Relaxed);
            self.c.gossip_batches.fetch_add(1, Relaxed);
            net.broadcast(NetMsg::Txs(std::mem::take(batch)));
        };
        for (tx, size) in buf {
            if !batch.is_empty()
                && (batch.len() >= self.cfg.gossip_max_batch_txs || bytes + size > self.cfg.gossip_max_batch_bytes)
            {
                send(&mut batch);
                bytes = 0;
            }
            bytes += size;
            batch.push(Arc::unwrap_or_clone(tx));
        }
        if !batch.is_empty() {
            send(&mut batch);
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes.load(Relaxed)
    }

    pub fn stats(&self) -> PoolStats {
        let c = &self.c;
        PoolStats {
            len: self.count.load(Relaxed),
            bytes: self.bytes.load(Relaxed),
            admitted: c.admitted.load(Relaxed),
            admitted_from_peers: c.admitted_from_peers.load(Relaxed),
            duplicate: c.duplicate.load(Relaxed),
            rate_limited_ip: c.rate_limited_ip.load(Relaxed),
            rate_limited_signer: c.rate_limited_signer.load(Relaxed),
            full: c.full.load(Relaxed),
            check_failed: c.check_failed.load(Relaxed),
            evicted_for_validator_txs: c.evicted.load(Relaxed),
            expired: c.expired.load(Relaxed),
            invalidated: c.invalidated.load(Relaxed),
            removed_committed: c.removed_committed.load(Relaxed),
            gossiped_txs: c.gossiped_txs.load(Relaxed),
            gossip_batches: c.gossip_batches.load(Relaxed),
        }
    }

    /// Exact count by walking all shards (tests / consistency checks).
    pub fn count_slow(&self) -> (usize, usize) {
        let mut n = 0;
        let mut b = 0;
        for sh in &self.shards {
            let s = sh.read();
            assert_eq!(s.map.len(), s.user_order.len() + s.val_order.len());
            n += s.map.len();
            b += s.map.values().map(|e| e.size).sum::<usize>();
        }
        (n, b)
    }
}

impl Mempool for Pool {
    fn insert(&self, tx: Tx, source: TxSource) -> Result<TxHash, AdmissionError> {
        self.insert_tx(tx, source)
    }
    fn reap(&self, max_txs: usize, max_bytes: usize) -> Vec<Tx> {
        self.reap_txs(max_txs, max_bytes)
    }
    fn remove_committed(&self, hashes: &[TxHash]) {
        self.remove_committed_txs(hashes)
    }
    fn len(&self) -> usize {
        self.count.load(Relaxed)
    }
}
