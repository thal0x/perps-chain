//! TCP p2p transport for hlbft validators: authenticated full mesh, borsh
//! framing, per-peer bounded outbound queues, consensus/tx inbound split, and
//! a runtime-configurable chaos shim. See `README.md`.
//!
//! Threading: `P2p` owns a private multi-threaded tokio runtime on its own
//! OS thread, so it can be created and used from synchronous code or from
//! inside another runtime. Inbound messages are delivered on **crossbeam**
//! channels (see [`Inbound`]), which suit a synchronous consensus loop
//! (`select!` over both receivers and a timer).

pub mod chaos;
pub mod handshake;
pub mod queue;

pub use chaos::{ChaosConfig, LinkChaos};

use bytes::Bytes;
use chaos::Chaos;
use crossbeam_channel as cb;
use handshake::{handshake, Identity};
use interfaces::{Address, NetMsg, Network, ValidatorSet};
use parking_lot::Mutex;
use queue::{OutQueue, PushResult};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Maximum encoded frame body (borsh(NetMsg)), bytes.
pub const MAX_FRAME: usize = 32 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("config: {0}")]
    Config(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Transport configuration.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetConfig {
    pub chain_id: u64,
    /// Validator secret key (raw 32 bytes). The local address is derived
    /// from it and must be a member of `validators`.
    #[serde(with = "hex_key")]
    pub secret_key: [u8; 32],
    pub validators: ValidatorSet,
    /// Address to bind (default: our own `p2p_addr` from the validator set).
    pub listen_addr: Option<String>,
    pub max_frame_bytes: usize,
    /// Per-peer outbound bound for tx-gossip frames (drop-oldest beyond it).
    pub tx_queue_max_bytes: usize,
    /// Per-peer outbound bound for consensus frames; exceeding it means the
    /// peer is not reading, and the connection is reset.
    pub consensus_queue_max_bytes: usize,
    /// Capacity (in messages) of the inbound tx channel; tx batches beyond
    /// it are dropped (counted). The consensus channel is unbounded.
    pub inbound_tx_capacity: usize,
    pub backoff_min_ms: u64,
    pub backoff_max_ms: u64,
    pub connect_timeout_ms: u64,
    pub handshake_timeout_ms: u64,
    pub worker_threads: usize,
    pub chaos: ChaosConfig,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            chain_id: 0,
            secret_key: [0u8; 32],
            validators: ValidatorSet { validators: vec![] },
            listen_addr: None,
            max_frame_bytes: MAX_FRAME,
            tx_queue_max_bytes: 64 << 20,
            consensus_queue_max_bytes: 1 << 30,
            inbound_tx_capacity: 4096,
            backoff_min_ms: 50,
            backoff_max_ms: 2000,
            connect_timeout_ms: 2000,
            handshake_timeout_ms: 5000,
            worker_threads: 2,
            chaos: ChaosConfig::default(),
        }
    }
}

impl std::fmt::Debug for NetConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetConfig")
            .field("chain_id", &self.chain_id)
            .field("secret_key", &"<redacted>")
            .field("validators", &self.validators)
            .field("listen_addr", &self.listen_addr)
            .finish_non_exhaustive()
    }
}

mod hex_key {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(k: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("0x{}", hex_encode(k)))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        let s = s.strip_prefix("0x").unwrap_or(&s);
        if s.len() != 64 {
            return Err(serde::de::Error::custom("secret key must be 32 bytes hex"));
        }
        let mut out = [0u8; 32];
        for (i, o) in out.iter_mut().enumerate() {
            *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(serde::de::Error::custom)?;
        }
        Ok(out)
    }
    fn hex_encode(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}

/// Inbound message channels returned by [`P2p::start`]. Consensus traffic
/// and tx gossip are split so a tx flood cannot starve consensus.
pub struct Inbound {
    /// `NetMsg::Consensus` messages (and loopback `send(local, ..)`), unbounded.
    pub consensus: cb::Receiver<(Address, NetMsg)>,
    /// `NetMsg::Txs` batches, bounded (`inbound_tx_capacity`); overflow dropped.
    pub txs: cb::Receiver<(Address, NetMsg)>,
}

/// Per-peer counters snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerMetrics {
    pub connected: bool,
    pub connects: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub msgs_in: u64,
    pub msgs_out: u64,
    /// Outbound tx frames dropped by backpressure (drop-oldest).
    pub tx_frames_dropped: u64,
    /// Inbound tx batches dropped because the inbound tx channel was full.
    pub inbound_tx_dropped: u64,
    /// Messages not sent because the peer was disconnected.
    pub dropped_disconnected: u64,
    pub chaos_dropped_out: u64,
    pub chaos_dropped_in: u64,
    pub queued_bytes: u64,
}

#[derive(Default)]
struct Counters {
    connects: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    msgs_in: AtomicU64,
    msgs_out: AtomicU64,
    tx_frames_dropped: AtomicU64,
    inbound_tx_dropped: AtomicU64,
    dropped_disconnected: AtomicU64,
    chaos_dropped_out: AtomicU64,
    chaos_dropped_in: AtomicU64,
}

struct Conn {
    id: u64,
    queue: OutQueue,
    cancel: CancellationToken,
}

struct PeerSlot {
    addr: Address,
    p2p_addr: String,
    /// We dial this peer (our address is lower).
    we_dial: bool,
    conn: Mutex<Option<Arc<Conn>>>,
    c: Counters,
}

struct Inner {
    cfg: NetConfig,
    id: Identity,
    peers: BTreeMap<Address, Arc<PeerSlot>>,
    chaos: Chaos,
    cons_tx: cb::Sender<(Address, NetMsg)>,
    txs_tx: cb::Sender<(Address, NetMsg)>,
    next_conn: AtomicU64,
    oversize_dropped: AtomicU64,
    shutdown: CancellationToken,
}

/// The p2p transport. Implements [`interfaces::Network`].
pub struct P2p {
    inner: Arc<Inner>,
    handle: tokio::runtime::Handle,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    stopped: AtomicBool,
}

fn encode_frame(msg: &NetMsg) -> Bytes {
    let mut v = Vec::with_capacity(256);
    v.extend_from_slice(&[0u8; 4]);
    borsh::to_writer(&mut v, msg).expect("vec write");
    let len = (v.len() - 4) as u32;
    v[..4].copy_from_slice(&len.to_be_bytes());
    Bytes::from(v)
}

impl P2p {
    /// Validate config, bind the listener, start the runtime and the dialers.
    pub fn start(cfg: NetConfig) -> Result<(Arc<P2p>, Inbound), NetError> {
        let id = Identity::new(&cfg.secret_key, cfg.chain_id).map_err(NetError::Config)?;
        let me = cfg
            .validators
            .validators
            .iter()
            .find(|v| v.address == id.address)
            .ok_or_else(|| NetError::Config(format!("local address {} not in validator set", id.address)))?;
        let listen = cfg.listen_addr.clone().unwrap_or_else(|| me.p2p_addr.clone());
        let listener = std::net::TcpListener::bind(&listen)?;
        Self::start_with_listener(cfg, listener)
    }

    /// Like [`start`](Self::start) with a pre-bound listener (tests use
    /// port 0 listeners to avoid port races).
    pub fn start_with_listener(cfg: NetConfig, listener: std::net::TcpListener) -> Result<(Arc<P2p>, Inbound), NetError> {
        let id = Identity::new(&cfg.secret_key, cfg.chain_id).map_err(NetError::Config)?;
        if !cfg.validators.contains(&id.address) {
            return Err(NetError::Config(format!("local address {} not in validator set", id.address)));
        }
        if cfg.chaos.validate().is_err() {
            return Err(NetError::Config("invalid chaos config".into()));
        }
        listener.set_nonblocking(true)?;
        let peers: BTreeMap<Address, Arc<PeerSlot>> = cfg
            .validators
            .validators
            .iter()
            .filter(|v| v.address != id.address)
            .map(|v| {
                (
                    v.address,
                    Arc::new(PeerSlot {
                        addr: v.address,
                        p2p_addr: v.p2p_addr.clone(),
                        we_dial: id.address < v.address,
                        conn: Mutex::new(None),
                        c: Counters::default(),
                    }),
                )
            })
            .collect();
        let (cons_tx, cons_rx) = cb::unbounded();
        let (txs_tx, txs_rx) = cb::bounded(cfg.inbound_tx_capacity.max(1));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(cfg.worker_threads.max(1))
            .thread_name("hlbft-net")
            .enable_all()
            .build()?;
        let handle = rt.handle().clone();
        let inner = Arc::new(Inner {
            chaos: Chaos::new(id.address, cfg.chaos.clone()),
            cfg,
            id,
            peers,
            cons_tx,
            txs_tx,
            next_conn: AtomicU64::new(1),
            oversize_dropped: AtomicU64::new(0),
            shutdown: CancellationToken::new(),
        });
        let listener = {
            let _g = handle.enter();
            TcpListener::from_std(listener)?
        };
        info!(local = %inner.id.address, addr = ?listener.local_addr().ok(), "p2p starting");
        handle.spawn(accept_loop(inner.clone(), listener));
        for slot in inner.peers.values().filter(|s| s.we_dial) {
            handle.spawn(dial_loop(inner.clone(), slot.clone()));
        }
        let stop = inner.shutdown.clone();
        let thread = std::thread::Builder::new()
            .name("hlbft-net-rt".into())
            .spawn(move || {
                rt.block_on(stop.cancelled());
                rt.shutdown_timeout(Duration::from_secs(1));
            })?;
        let p2p = Arc::new(P2p { inner, handle, thread: Mutex::new(Some(thread)), stopped: AtomicBool::new(false) });
        Ok((p2p, Inbound { consensus: cons_rx, txs: txs_rx }))
    }

    /// Stop all tasks, close the listener and every connection. Idempotent.
    /// Blocks until the runtime thread exits (do not call from a task of
    /// this transport's own runtime).
    pub fn shutdown(&self) {
        if self.stopped.swap(true, Relaxed) {
            return;
        }
        self.inner.shutdown.cancel();
        if let Some(t) = self.thread.lock().take() {
            let _ = t.join();
        }
        for s in self.inner.peers.values() {
            if let Some(c) = s.conn.lock().take() {
                c.queue.close();
            }
        }
    }

    /// Replace the chaos configuration (takes effect immediately).
    pub fn set_chaos(&self, cfg: ChaosConfig) -> Result<(), String> {
        info!(local = %self.inner.id.address, ?cfg, "chaos config updated");
        self.inner.chaos.set(cfg)
    }

    /// Parse and apply a JSON [`ChaosConfig`].
    pub fn set_chaos_json(&self, json: &str) -> Result<(), String> {
        self.set_chaos(ChaosConfig::from_json(json)?)
    }

    pub fn chaos(&self) -> ChaosConfig {
        self.inner.chaos.get()
    }

    /// Per-peer counters.
    pub fn metrics(&self) -> BTreeMap<Address, PeerMetrics> {
        self.inner
            .peers
            .values()
            .map(|s| {
                let conn = s.conn.lock().clone();
                let c = &s.c;
                (
                    s.addr,
                    PeerMetrics {
                        connected: conn.is_some(),
                        connects: c.connects.load(Relaxed),
                        bytes_in: c.bytes_in.load(Relaxed),
                        bytes_out: c.bytes_out.load(Relaxed),
                        msgs_in: c.msgs_in.load(Relaxed),
                        msgs_out: c.msgs_out.load(Relaxed),
                        tx_frames_dropped: c.tx_frames_dropped.load(Relaxed),
                        inbound_tx_dropped: c.inbound_tx_dropped.load(Relaxed),
                        dropped_disconnected: c.dropped_disconnected.load(Relaxed),
                        chaos_dropped_out: c.chaos_dropped_out.load(Relaxed),
                        chaos_dropped_in: c.chaos_dropped_in.load(Relaxed),
                        queued_bytes: conn.map(|c| c.queue.queued_bytes() as u64).unwrap_or(0),
                    },
                )
            })
            .collect()
    }

    /// Bytes currently queued for `peer` (0 if not connected).
    pub fn queued_bytes(&self, peer: &Address) -> usize {
        self.inner.peers.get(peer).and_then(|s| s.conn.lock().clone()).map(|c| c.queue.queued_bytes()).unwrap_or(0)
    }

    /// Messages refused because their encoding exceeded `max_frame_bytes`.
    pub fn oversize_dropped(&self) -> u64 {
        self.inner.oversize_dropped.load(Relaxed)
    }

    fn dispatch(&self, to: &[Address], msg: NetMsg) {
        let is_tx = matches!(msg, NetMsg::Txs(_));
        let frame = encode_frame(&msg);
        if frame.len() - 4 > self.inner.cfg.max_frame_bytes {
            warn!(len = frame.len() - 4, "dropping oversize outbound message");
            self.inner.oversize_dropped.fetch_add(1, Relaxed);
            return;
        }
        for addr in to {
            let Some(slot) = self.inner.peers.get(addr) else { continue };
            let copies = self.inner.chaos.outbound(addr);
            if copies.is_empty() {
                slot.c.chaos_dropped_out.fetch_add(1, Relaxed);
                continue;
            }
            for &d in copies.as_slice() {
                if d == 0 {
                    enqueue(&self.inner, slot, frame.clone(), is_tx);
                } else {
                    let inner = self.inner.clone();
                    let slot = slot.clone();
                    let frame = frame.clone();
                    self.handle.spawn(async move {
                        tokio::time::sleep(Duration::from_millis(d)).await;
                        enqueue(&inner, &slot, frame, is_tx);
                    });
                }
            }
        }
    }
}

impl Drop for P2p {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn enqueue(inner: &Inner, slot: &PeerSlot, frame: Bytes, is_tx: bool) {
    if inner.chaos.is_cut(&slot.addr) {
        slot.c.chaos_dropped_out.fetch_add(1, Relaxed);
        return;
    }
    let Some(conn) = slot.conn.lock().clone() else {
        slot.c.dropped_disconnected.fetch_add(1, Relaxed);
        return;
    };
    match conn.queue.push(frame, is_tx) {
        PushResult::Ok { dropped_tx_frames } => {
            if dropped_tx_frames > 0 {
                slot.c.tx_frames_dropped.fetch_add(dropped_tx_frames as u64, Relaxed);
            }
        }
        PushResult::Closed => {
            slot.c.dropped_disconnected.fetch_add(1, Relaxed);
        }
        PushResult::Overflow => {
            warn!(peer = %slot.addr, "consensus outbound queue overflow; resetting connection");
            slot.c.dropped_disconnected.fetch_add(1, Relaxed);
            conn.cancel.cancel();
        }
    }
}

impl Network for P2p {
    fn local(&self) -> Address {
        self.inner.id.address
    }

    /// Send to one validator. `send(local, msg)` loops back to our own
    /// inbound channels (not subject to chaos).
    fn send(&self, to: Address, msg: NetMsg) {
        if to == self.inner.id.address {
            deliver(&self.inner, None, to, msg);
            return;
        }
        self.dispatch(&[to], msg);
    }

    /// Send to every other validator (not to ourselves). Encoded once.
    fn broadcast(&self, msg: NetMsg) {
        let to: Vec<Address> = self.inner.peers.keys().copied().collect();
        self.dispatch(&to, msg);
    }

    fn peers(&self) -> Vec<Address> {
        self.inner.peers.values().filter(|s| s.conn.lock().is_some()).map(|s| s.addr).collect()
    }
}

fn deliver(inner: &Inner, slot: Option<&PeerSlot>, from: Address, msg: NetMsg) {
    match msg {
        NetMsg::Consensus(_) => {
            let _ = inner.cons_tx.send((from, msg));
        }
        NetMsg::Txs(_) => {
            if inner.txs_tx.try_send((from, msg)).is_err() {
                if let Some(s) = slot {
                    s.c.inbound_tx_dropped.fetch_add(1, Relaxed);
                }
            }
        }
    }
}

// ------------------------------------------------------------ connections ---

async fn accept_loop(inner: Arc<Inner>, listener: TcpListener) {
    loop {
        let res = tokio::select! {
            _ = inner.shutdown.cancelled() => return,
            r = listener.accept() => r,
        };
        match res {
            Ok((stream, remote)) => {
                let inner = inner.clone();
                tokio::spawn(async move {
                    let me = inner.id.address;
                    let accept = |p: &Address| inner.peers.get(p).map(|s| !s.we_dial).unwrap_or(false) && *p < me;
                    match setup(&inner, stream, accept).await {
                        Ok((peer, stream)) => {
                            let slot = inner.peers[&peer].clone();
                            run_conn(inner.clone(), slot, stream).await;
                        }
                        Err(e) => debug!(%remote, "inbound handshake failed: {e}"),
                    }
                });
            }
            Err(e) => {
                warn!("accept error: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

async fn setup<F: FnOnce(&Address) -> bool>(
    inner: &Inner,
    mut stream: TcpStream,
    accept: F,
) -> Result<(Address, TcpStream), String> {
    let _ = stream.set_nodelay(true);
    let t = Duration::from_millis(inner.cfg.handshake_timeout_ms);
    match tokio::time::timeout(t, handshake(&mut stream, &inner.id, accept)).await {
        Ok(Ok(peer)) => Ok((peer, stream)),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("handshake timeout".into()),
    }
}

async fn dial_loop(inner: Arc<Inner>, slot: Arc<PeerSlot>) {
    let min = Duration::from_millis(inner.cfg.backoff_min_ms.max(1));
    let max = Duration::from_millis(inner.cfg.backoff_max_ms.max(inner.cfg.backoff_min_ms.max(1)));
    let mut backoff = min;
    loop {
        if inner.shutdown.is_cancelled() {
            return;
        }
        let ct = Duration::from_millis(inner.cfg.connect_timeout_ms);
        let attempt = async {
            let s = tokio::time::timeout(ct, TcpStream::connect(&slot.p2p_addr))
                .await
                .map_err(|_| "connect timeout".to_string())?
                .map_err(|e| e.to_string())?;
            let expect = slot.addr;
            setup(&inner, s, |p| *p == expect).await
        };
        let res = tokio::select! {
            _ = inner.shutdown.cancelled() => return,
            r = attempt => r,
        };
        match res {
            Ok((_, stream)) => {
                backoff = min;
                tokio::select! {
                    _ = inner.shutdown.cancelled() => return,
                    _ = run_conn(inner.clone(), slot.clone(), stream) => {}
                }
            }
            Err(e) => debug!(peer = %slot.addr, "dial failed: {e}"),
        }
        let jitter = Duration::from_millis(rand::random::<u64>() % (backoff.as_millis() as u64 / 4 + 1));
        tokio::select! {
            _ = inner.shutdown.cancelled() => return,
            _ = tokio::time::sleep(backoff + jitter) => {}
        }
        backoff = (backoff * 2).min(max);
    }
}

/// Install a freshly authenticated connection (replacing any previous one to
/// the same peer) and run it until it fails or is replaced.
async fn run_conn(inner: Arc<Inner>, slot: Arc<PeerSlot>, stream: TcpStream) {
    let conn = Arc::new(Conn {
        id: inner.next_conn.fetch_add(1, Relaxed),
        queue: OutQueue::new(inner.cfg.tx_queue_max_bytes, inner.cfg.consensus_queue_max_bytes),
        cancel: inner.shutdown.child_token(),
    });
    if let Some(old) = slot.conn.lock().replace(conn.clone()) {
        old.cancel.cancel();
        old.queue.close();
    }
    slot.c.connects.fetch_add(1, Relaxed);
    info!(local = %inner.id.address, peer = %slot.addr, "peer connected");
    let (rd, wr) = stream.into_split();
    let reader = read_loop(&inner, &slot, rd);
    let writer = write_loop(&inner, &slot, &conn, wr);
    tokio::select! {
        _ = conn.cancel.cancelled() => {}
        r = reader => debug!(peer = %slot.addr, "reader ended: {r:?}"),
        r = writer => debug!(peer = %slot.addr, "writer ended: {r:?}"),
    }
    conn.cancel.cancel();
    conn.queue.close();
    let mut cur = slot.conn.lock();
    if cur.as_ref().map(|c| c.id) == Some(conn.id) {
        *cur = None;
        info!(local = %inner.id.address, peer = %slot.addr, "peer disconnected");
    }
}

async fn read_loop(inner: &Inner, slot: &PeerSlot, rd: tokio::net::tcp::OwnedReadHalf) -> std::io::Result<()> {
    let mut r = BufReader::with_capacity(256 * 1024, rd);
    loop {
        let len = r.read_u32().await? as usize;
        if len > inner.cfg.max_frame_bytes {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("frame too large: {len}")));
        }
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf).await?;
        slot.c.bytes_in.fetch_add(len as u64 + 4, Relaxed);
        slot.c.msgs_in.fetch_add(1, Relaxed);
        let msg: NetMsg = borsh::from_slice(&buf)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad frame: {e}")))?;
        drop(buf);
        if inner.chaos.is_cut(&slot.addr) {
            slot.c.chaos_dropped_in.fetch_add(1, Relaxed);
            continue;
        }
        deliver(inner, Some(slot), slot.addr, msg);
    }
}

async fn write_loop(
    inner: &Inner,
    slot: &PeerSlot,
    conn: &Conn,
    wr: tokio::net::tcp::OwnedWriteHalf,
) -> std::io::Result<()> {
    let mut w = BufWriter::with_capacity(256 * 1024, wr);
    let mut batch = Vec::new();
    loop {
        if !conn.queue.pop_batch(4 << 20, &mut batch).await {
            return Ok(());
        }
        for f in batch.drain(..) {
            if inner.chaos.is_cut(&slot.addr) {
                slot.c.chaos_dropped_out.fetch_add(1, Relaxed);
                continue;
            }
            w.write_all(&f).await?;
            slot.c.bytes_out.fetch_add(f.len() as u64, Relaxed);
            slot.c.msgs_out.fetch_add(1, Relaxed);
        }
        w.flush().await?;
    }
}
