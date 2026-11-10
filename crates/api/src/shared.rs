//! Shared server state: backend handle, indexer, receipt waiters, WS hub.

use crate::coins::Coins;
use crate::indexer::{Indexer, IndexerLimits};
use crate::render;
use crate::ws::Hub;
use interfaces::*;
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

#[derive(Clone, Debug)]
pub struct ApiConfig {
    /// Max time `/exchange` waits for the receipt of a submitted tx.
    pub exchange_timeout: Duration,
    /// Outbound WS queue per connection (messages); a full queue drops the
    /// connection (slow consumer).
    pub ws_buffer: usize,
    /// Max subscriptions per WS connection.
    pub ws_max_subs: usize,
    pub limits: IndexerLimits,
    /// Recent receipts kept for duplicate submissions.
    pub receipt_cache: usize,
    /// Max request body (bytes).
    pub max_body: usize,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            exchange_timeout: Duration::from_secs(10),
            ws_buffer: 4096,
            ws_max_subs: 1000,
            limits: IndexerLimits::default(),
            receipt_cache: 100_000,
            max_body: 8 << 20,
        }
    }
}

const SHARDS: usize = 64;

type WaiterMap = HashMap<TxHash, Vec<oneshot::Sender<Arc<TxReceipt>>>>;

/// tx hash -> waiting `/exchange` requests, sharded to avoid a global lock.
pub struct Waiters {
    shards: Vec<Mutex<WaiterMap>>,
    recent: Mutex<(HashMap<TxHash, Arc<TxReceipt>>, VecDeque<TxHash>)>,
    cap: usize,
}

impl Waiters {
    pub fn new(cap: usize) -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
            recent: Mutex::new((HashMap::new(), VecDeque::new())),
            cap,
        }
    }

    fn shard(&self, h: &TxHash) -> &Mutex<WaiterMap> {
        &self.shards[(h.0[0] as usize ^ ((h.0[1] as usize) << 8)) % SHARDS]
    }

    pub fn register(&self, h: TxHash) -> oneshot::Receiver<Arc<TxReceipt>> {
        let (tx, rx) = oneshot::channel();
        self.shard(&h).lock().entry(h).or_default().push(tx);
        rx
    }

    /// Drop waiters whose receivers are gone.
    pub fn cancel(&self, h: &TxHash) {
        let mut s = self.shard(h).lock();
        if let Some(v) = s.get_mut(h) {
            v.retain(|t| !t.is_closed());
            if v.is_empty() {
                s.remove(h);
            }
        }
    }

    pub fn pending(&self) -> usize {
        self.shards.iter().map(|s| s.lock().len()).sum()
    }

    pub fn resolve(&self, receipts: &[TxReceipt]) {
        if receipts.is_empty() {
            return;
        }
        let mut arcs = Vec::with_capacity(receipts.len());
        for r in receipts {
            let r = Arc::new(r.clone());
            if let Some(v) = self.shard(&r.tx_hash).lock().remove(&r.tx_hash) {
                for t in v {
                    let _ = t.send(r.clone());
                }
            }
            arcs.push(r);
        }
        let mut rc = self.recent.lock();
        for r in arcs {
            if rc.0.insert(r.tx_hash, r.clone()).is_none() {
                rc.1.push_back(r.tx_hash);
            }
        }
        while rc.1.len() > self.cap {
            if let Some(h) = rc.1.pop_front() {
                rc.0.remove(&h);
            }
        }
    }

    pub fn recent(&self, h: &TxHash) -> Option<Arc<TxReceipt>> {
        self.recent.lock().0.get(h).cloned()
    }
}

pub struct Shared {
    pub backend: Arc<dyn ApiBackend>,
    pub genesis: Genesis,
    pub coins: Coins,
    pub index: RwLock<Indexer>,
    pub waiters: Waiters,
    pub hub: Hub,
    pub cfg: ApiConfig,
}

impl Shared {
    /// Build the shared state, start the indexer thread and register it with
    /// the backend's block stream.
    pub fn start(backend: Arc<dyn ApiBackend>, cfg: ApiConfig) -> Arc<Self> {
        let genesis = backend.genesis();
        let coins = Coins::new(&genesis);
        let shared = Arc::new(Shared {
            index: RwLock::new(Indexer::new(cfg.limits.clone())),
            waiters: Waiters::new(cfg.receipt_cache),
            hub: Hub::default(),
            backend: backend.clone(),
            genesis,
            coins,
            cfg,
        });
        let (tx, rx) = crossbeam_channel::unbounded::<Arc<BlockOutcome>>();
        backend.on_block(Box::new(move |b| {
            let _ = tx.send(b);
        }));
        let s = shared.clone();
        std::thread::Builder::new()
            .name("api-indexer".into())
            .spawn(move || {
                while let Ok(b) = rx.recv() {
                    // Never let a bad block kill the indexer thread.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.process_block(&b)));
                    if r.is_err() {
                        tracing::error!(height = b.height, "api indexer panicked on block");
                        // still release waiters
                        s.waiters.resolve(&b.receipts);
                    }
                }
            })
            .expect("spawn api indexer thread");
        shared
    }

    fn process_block(&self, b: &BlockOutcome) {
        let _g = self.hub.publish_lock.lock();
        let has_funding = b.events.iter().any(|e| matches!(e, Event::Funding { .. }));
        let premiums: HashMap<AssetId, Rate> = if has_funding {
            self.with_state(|q| self.coins.markets.iter().map(|m| (m.asset, q.market_ctx(m.asset).premium)).collect())
        } else {
            HashMap::new()
        };
        self.index.write().ingest(b, &|a| premiums.get(&a).copied().unwrap_or(Rate::ZERO));
        // Resolve after indexing so a client's follow-up /info sees the block.
        self.waiters.resolve(&b.receipts);
        // The WS delta is only built when someone listens. Subscribing takes
        // `publish_lock` (held here) for snapshot + registration, so a new
        // subscriber either sees this block in its snapshot or as a delta.
        if !self.hub.is_empty() {
            let delta = crate::indexer::BlockDelta::build(b);
            self.hub.publish(self, b, &delta);
        }
    }

    /// "Server time" for responses: the latest executed block time (falls
    /// back to the wall clock before the first block).
    pub fn now(&self) -> u64 {
        let t = self.with_state(|q| q.block_time());
        if t == 0 {
            crate::fmt::now_ms()
        } else {
            t
        }
    }

    /// Run `f` against the latest executed state.
    pub fn with_state<R>(&self, f: impl FnOnce(&dyn StateQuery) -> R) -> R {
        let mut f = Some(f);
        let mut out = None;
        self.backend.query(&mut |q| {
            if let Some(f) = f.take() {
                out = Some(f(q));
            }
        });
        out.expect("ApiBackend::query must invoke the callback")
    }

    /// `/exchange` body (parsed JSON, e.g. a WS `post`) -> HL response JSON.
    pub async fn exchange(self: &Arc<Self>, body: &Value, ip: String) -> String {
        let body = body.clone();
        let s = self.clone();
        let prep = tokio::task::spawn_blocking(move || match crate::wire::parse_exchange_request(&body) {
            Ok(u) => s.submit(u, ip),
            Err(e) => Err(render::err_string(&e)),
        })
        .await;
        self.finish(prep).await
    }

    /// `/exchange` raw body bytes -> HL response JSON (fast parse path).
    /// Parsing, hashing and `ApiBackend::submit` (signature recovery and
    /// mempool admission) run on the blocking pool so they never starve the
    /// async workers serving /info and WebSocket traffic.
    pub async fn exchange_bytes(self: &Arc<Self>, body: axum::body::Bytes, ip: String) -> String {
        let s = self.clone();
        let prep = tokio::task::spawn_blocking(move || match crate::wire::parse_exchange_bytes(&body) {
            Ok(u) => s.submit(u, ip),
            Err(e) => Err(render::err_string(&e)),
        })
        .await;
        self.finish(prep).await
    }

    async fn finish(&self, prep: Result<Result<Pending, String>, tokio::task::JoinError>) -> String {
        match prep {
            Ok(Ok(p)) => self.wait(p).await,
            Ok(Err(resp)) => resp,
            Err(_) => render::err_string("internal error"),
        }
    }

    /// Register the receipt waiter, then submit. `Err` is a final response.
    fn submit(&self, utx: UserTx, ip: String) -> Result<Pending, String> {
        let tx = Tx::User(utx);
        let mut h = tx.hash();
        let mut rx = self.waiters.register(h);
        match self.backend.submit(tx, TxSource::Api { ip }) {
            Ok(ret) => {
                if ret != h {
                    self.waiters.cancel(&h);
                    drop(rx);
                    h = ret;
                    rx = self.waiters.register(h);
                    if let Some(r) = self.waiters.recent(&h) {
                        drop(rx);
                        self.waiters.cancel(&h);
                        return Err(render::action_result_string(&r.result));
                    }
                }
            }
            Err(AdmissionError::Duplicate) => {
                if let Some(r) = self.waiters.recent(&h) {
                    drop(rx);
                    self.waiters.cancel(&h);
                    return Err(render::action_result_string(&r.result));
                }
                // else: the original is still pending; wait for its receipt.
            }
            Err(e) => {
                drop(rx);
                self.waiters.cancel(&h);
                return Err(render::err_string(&admission_message(&e)));
            }
        }
        Ok(Pending { hash: h, rx })
    }

    async fn wait(&self, p: Pending) -> String {
        match tokio::time::timeout(self.cfg.exchange_timeout, p.rx).await {
            Ok(Ok(r)) => render::action_result_string(&r.result),
            _ => {
                self.waiters.cancel(&p.hash);
                render::err_string("Timed out waiting for the transaction to be included in a block")
            }
        }
    }
}

/// A submitted tx waiting for its receipt.
struct Pending {
    hash: TxHash,
    rx: oneshot::Receiver<Arc<TxReceipt>>,
}

/// HL-style text for admission failures.
pub fn admission_message(e: &AdmissionError) -> String {
    match e {
        AdmissionError::UnknownSigner(a) => format!("User or API Wallet {} does not exist.", a.to_hex()),
        AdmissionError::BadSignature => "Invalid signature".into(),
        AdmissionError::BadNonce(s) => {
            if s.to_lowercase().contains("nonce") {
                s.clone()
            } else {
                format!("Invalid nonce: {s}")
            }
        }
        AdmissionError::RateLimited => "Too many requests".into(),
        AdmissionError::MempoolFull => "Mempool full, try again later".into(),
        AdmissionError::Duplicate => "Duplicate transaction".into(),
        AdmissionError::Malformed(s) => s.clone(),
        AdmissionError::Other(s) => s.clone(),
    }
}
