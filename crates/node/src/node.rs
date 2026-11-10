//! `hlnode run`: wires p2p, mempool, consensus, executor, API, oracle feeder
//! and bridge watcher together. See README.md for the thread layout.

use crate::config::{FeedSpec, Invariants, Resolved};
use crate::inflight::InFlight;
use crate::logs::{commit_line, hex32, root_line, HeightLog};
use crate::replay::ReplaySource;
use anyhow::{anyhow, Context, Result};
use consensus::{CoreConfig, Driver, DriverEvent, DriverHandle, DriverStatus, FileStore, FileStoreConfig};
use crossbeam_channel::{Receiver, Sender};
use hlwire::Wallet;
use interfaces::*;
use mempool::{Pool, PoolDeps};
use net::{NetConfig, P2p};
use parking_lot::{Mutex, RwLock};
use state::{ChainConfig, HlChain};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{error, info, warn};

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Roots the executor produced, readable by the consensus env without
/// touching the (possibly write-locked) chain.
pub struct ExecInfo {
    pub height: Height,
    pub root: Hash32,
    pub genesis_root: Hash32,
    pub roots: BTreeMap<Height, Hash32>,
}

impl ExecInfo {
    fn set(&mut self, h: Height, root: Hash32) {
        self.height = h;
        self.root = root;
        self.roots.insert(h, root);
        if self.roots.len() > 12_000 {
            let cut = h.saturating_sub(10_000);
            self.roots = self.roots.split_off(&cut);
        }
    }
    fn reset(&mut self, h: Height, root: Hash32) {
        self.roots.clear();
        self.set(h, root);
    }
    pub fn root_at(&self, h: Height) -> Option<Hash32> {
        if h == 0 {
            return Some(self.genesis_root);
        }
        self.roots.get(&h).copied()
    }
}

/// State shared by every node thread.
pub struct Shared {
    pub genesis: Genesis,
    pub wallet: Wallet,
    pub chain: RwLock<HlChain>,
    pub sig_cache: Arc<state::SigCache>,
    pub verifier: hlwire::HlVerifier,
    pub pool: Arc<Pool>,
    pub p2p: Arc<P2p>,
    pub exec: Mutex<ExecInfo>,
    pub inflight: InFlight,
    pub callbacks: RwLock<Vec<BlockCallback>>,
    pub driver_status: Mutex<Option<Arc<DriverStatus>>>,
    pub last_commit_ms: AtomicU64,
    /// Serialises validator-tx nonce allocation + mempool insertion so our
    /// validator txs enter the pool in nonce order.
    vnonce: Mutex<u64>,
}

impl Shared {
    /// Decode + recover a user tx outside any chain lock and put the result
    /// in the chain's signature cache (so `check_tx` and execution hit it).
    pub fn precheck(&self, tx: &Tx) {
        if let Tx::User(u) = tx {
            let h = tx.hash();
            if self.sig_cache.get(&h).is_some() {
                return;
            }
            let r = match self.verifier.decode(u) {
                Err(e) => Err(format!("Failed to decode action: {e}")),
                Ok(a) => match self.verifier.recover_signer(u, &a) {
                    Ok(s) => Ok((a, s)),
                    Err(_) => Err(state::errors::INVALID_SIGNATURE.to_string()),
                },
            };
            self.sig_cache.insert(h, Arc::new(r));
        }
    }

    pub fn check_tx(&self, tx: &Tx) -> Result<Address, AdmissionError> {
        self.precheck(tx);
        self.chain.read().check_tx(tx)
    }

    /// Sign a validator action with our key and insert it locally.
    pub fn submit_validator_action(&self, a: ValidatorAction) -> Result<TxHash, AdmissionError> {
        let mut g = self.vnonce.lock();
        let n = (*g + 1).max(now_ms());
        *g = n;
        let tx = hlwire::sign_validator_tx(&self.wallet, self.genesis.chain_id, n, a);
        self.pool.insert_prechecked(Tx::Validator(tx), TxSource::Local, Some(self.wallet.address()))
    }

    pub fn status(&self) -> NodeStatus {
        let (committed, view) = match &*self.driver_status.lock() {
            Some(s) => (s.committed_height.load(Ordering::Relaxed), s.view.load(Ordering::Relaxed)),
            None => (0, 0),
        };
        let (eh, er) = {
            let e = self.exec.lock();
            (e.height, e.root)
        };
        NodeStatus {
            validator: Some(self.wallet.address()),
            committed_height: committed,
            executed_height: eh,
            view,
            exec_root: er,
            mempool_len: interfaces::Mempool::len(&*self.pool),
            peers: self.p2p.peers(),
            last_commit_ms: self.last_commit_ms.load(Ordering::Relaxed),
        }
    }
}

// ------------------------------------------------------------ api backend --

pub struct Backend(pub Arc<Shared>);

impl ApiBackend for Backend {
    fn genesis(&self) -> Genesis {
        self.0.genesis.clone()
    }
    fn query(&self, f: &mut dyn FnMut(&dyn StateQuery)) {
        let c = self.0.chain.read();
        f(c.query())
    }
    fn submit(&self, tx: Tx, source: TxSource) -> Result<TxHash, AdmissionError> {
        match &tx {
            Tx::User(_) => {
                let signer = self.0.check_tx(&tx)?;
                self.0.pool.insert_prechecked(tx, source, Some(signer))
            }
            Tx::Validator(_) => interfaces::Mempool::insert(&*self.0.pool, tx, source),
        }
    }
    fn on_block(&self, cb: BlockCallback) {
        self.0.callbacks.write().push(cb);
    }
    fn status(&self) -> NodeStatus {
        self.0.status()
    }
    fn set_chaos(&self, cfg_json: &str) -> Result<(), String> {
        self.0.p2p.set_chaos_json(cfg_json)
    }
}

impl BridgeHost for Backend {
    fn validator(&self) -> Option<Address> {
        Some(self.0.wallet.address())
    }
    fn submit_validator_action(&self, a: ValidatorAction) -> Result<(), String> {
        self.0.submit_validator_action(a).map(|_| ()).map_err(|e| e.to_string())
    }
    fn pending_withdrawals(&self) -> Vec<PendingWithdrawal> {
        self.0.chain.read().query().pending_withdrawals()
    }
    fn processed_deposit(&self, deposit_id: u64) -> bool {
        self.0.chain.read().query().processed_deposit(deposit_id)
    }
    fn sign_digest(&self, digest: &[u8; 32]) -> Option<Sig65> {
        Some(self.0.wallet.sign_digest(digest))
    }
    fn genesis(&self) -> Genesis {
        self.0.genesis.clone()
    }
}

// -------------------------------------------------------- consensus env --

pub struct NodeEnv {
    shared: Arc<Shared>,
    store: FileStore,
    sk: secp256k1::SecretKey,
    replay: Option<ReplaySource>,
}

impl ConsensusEnv for NodeEnv {
    fn now_ms(&self) -> u64 {
        now_ms()
    }
    fn payload(&mut self, max_txs: usize, max_bytes: usize) -> Vec<Tx> {
        let p = &self.shared.genesis.params;
        let max_txs = max_txs.min(p.max_block_txs as usize);
        let max_bytes = max_bytes.min(p.max_block_bytes as usize);
        let skip = self.shared.inflight.len();
        let reaped = interfaces::Mempool::reap(
            &*self.shared.pool,
            max_txs.saturating_add(skip),
            max_bytes.saturating_add(max_bytes.min(skip.saturating_mul(4096))),
        );
        let items: Vec<(TxHash, (usize, Tx))> = reaped
            .into_iter()
            .map(|tx| {
                let enc = borsh::to_vec(&tx).expect("borsh");
                (blake3_hash(&enc), (enc.len(), tx))
            })
            .collect();
        let items = self.shared.inflight.retain_absent(items);
        let mut out = Vec::with_capacity(items.len().min(max_txs));
        let mut hashes = Vec::with_capacity(out.capacity());
        let mut bytes = 0usize;
        for (h, (len, tx)) in items {
            if out.len() >= max_txs {
                break;
            }
            if bytes + len > max_bytes {
                continue;
            }
            bytes += len;
            hashes.push(h);
            out.push(tx);
        }
        let exp = self.shared.inflight.own_expiry();
        self.shared.inflight.add(exp, hashes);
        out
    }
    fn exec_state(&self) -> (Height, Hash32) {
        let e = self.shared.exec.lock();
        (e.height, e.root)
    }
    fn root_at(&self, h: Height) -> Option<Hash32> {
        self.shared.exec.lock().root_at(h)
    }
    fn snapshot(&self) -> Option<Snapshot> {
        self.store.latest_snapshot()
    }
    fn proposal_override(&mut self, height: Height) -> Option<(TimestampMs, Vec<Tx>)> {
        self.replay.as_mut().and_then(|r| r.get(height))
    }
    fn sign(&self, digest: &[u8; 32]) -> Sig65 {
        consensus::crypto::secp::sign(&self.sk, digest)
    }
    fn verify(&self, digest: &[u8; 32], sig: &Sig65, signer: &Address) -> bool {
        consensus::crypto::secp::verify(digest, sig, signer)
    }
    fn store(&mut self) -> &mut dyn BlockStore {
        &mut self.store
    }
}

// --------------------------------------------------------------- executor --

#[allow(clippy::large_enum_variant)]
enum ExecMsg {
    Commit(CommittedBlock),
    Install(Snapshot),
}

struct Executor {
    shared: Arc<Shared>,
    roots_log: HeightLog,
    driver: Sender<DriverEvent>,
    snapshot_interval: u64,
    /// Replay mode: last logged height (padding blocks beyond it are not executed).
    replay_end: Option<Height>,
    divergences: u64,
}

impl Executor {
    fn run(mut self, rx: Receiver<ExecMsg>) {
        while let Ok(m) = rx.recv() {
            match m {
                ExecMsg::Commit(cb) => self.execute(cb),
                ExecMsg::Install(s) => self.install(s),
            }
        }
    }

    fn install(&mut self, s: Snapshot) {
        let r = self.shared.chain.write().restore(&s);
        match r {
            Ok(root) => {
                if root != s.state_root {
                    error!(height = s.height, "STATE DIVERGENCE: snapshot root mismatch after restore");
                }
                info!(height = s.height, root = %hex32(&root), "installed state snapshot");
                self.shared.exec.lock().reset(s.height, root);
                let _ = self.driver.send(DriverEvent::Executed { height: s.height, root });
            }
            Err(e) => error!(height = s.height, "snapshot restore failed: {e}"),
        }
    }

    fn execute(&mut self, cb: CommittedBlock) {
        let b = &cb.block;
        let h = b.header.height;
        let hashes: Vec<TxHash> = b.payload.iter().map(|t| t.hash()).collect();
        interfaces::Mempool::remove_committed(&*self.shared.pool, &hashes);
        let executed = self.shared.chain.read().executed_height();
        if h <= executed {
            return;
        }
        if h != executed + 1 {
            error!(height = h, executed, "executor gap: cannot execute out of order");
            return;
        }
        if let Some(end) = self.replay_end {
            if h > end {
                return;
            }
        }
        let ctx = BlockContext {
            height: h,
            timestamp_ms: b.header.timestamp_ms,
            proposer: b.header.proposer,
            block_id: b.id(),
        };
        let (outcome, snap) = {
            let mut c = self.shared.chain.write();
            let o = c.execute_block(&ctx, &b.payload);
            let v = c.invariant_violations().len();
            if v > 0 && h.is_multiple_of(100) {
                warn!(height = h, violations = v, "invariant violations recorded");
            }
            let snap = if h.is_multiple_of(self.snapshot_interval) {
                let c = parking_lot::RwLockWriteGuard::downgrade(c);
                Some(c.snapshot())
            } else {
                None
            };
            (o, snap)
        };
        let root = outcome.state_root;
        self.shared.exec.lock().set(h, root);
        let _ = self.driver.send(DriverEvent::Executed { height: h, root });
        if let Some(s) = snap {
            let _ = self.driver.send(DriverEvent::SaveSnapshot(s));
        }
        // roots.log: append, or verify against the previous run's entry
        match self.roots_log.get(h) {
            Some(prev) if prev != hex32(&root) => {
                self.divergences += 1;
                error!(height = h, previous = prev, now = %hex32(&root), "STATE DIVERGENCE: re-executed root differs from roots.log");
            }
            Some(_) => {}
            None => {
                if let Err(e) = self.roots_log.append(h, &root_line(h, &root, outcome.order_actions)) {
                    error!("roots.log write failed: {e}");
                }
            }
        }
        let outcome = Arc::new(outcome);
        for cb in self.shared.callbacks.read().iter() {
            cb(outcome.clone());
        }
    }
}

// ------------------------------------------------------------- the node --

pub struct Node {
    pub shared: Arc<Shared>,
    driver: Option<DriverHandle>,
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    runtime: Option<tokio::runtime::Runtime>,
}

fn read_key(path: &PathBuf) -> Result<Wallet> {
    let s = std::fs::read_to_string(path).with_context(|| format!("read key {}", path.display()))?;
    Wallet::from_secret_hex(s.trim()).map_err(|e| anyhow!("bad key {}: {e}", path.display()))
}

impl Node {
    pub fn start(cfg: Resolved) -> Result<Node> {
        let genesis: Genesis = serde_json::from_slice(
            &std::fs::read(&cfg.genesis_path).with_context(|| format!("read genesis {}", cfg.genesis_path.display()))?,
        )
        .context("parse genesis.json")?;
        let wallet = read_key(&cfg.key_path)?;
        let me = wallet.address();
        anyhow::ensure!(genesis.validators.contains(&me), "key {me} is not a genesis validator");
        let my_index = genesis.validators.validators.iter().position(|v| v.address == me).unwrap_or(0);
        info!(validator = %me, index = my_index, home = %cfg.home.display(), "starting hlnode");

        let replay = match &cfg.replay_log {
            Some(p) => Some(ReplaySource::open(p, &genesis)?),
            None => None,
        };
        let replay_end = replay.as_ref().map(|r| r.last_height());

        // ---- state (restored from the latest snapshot if any)
        let data = cfg.home.join("data");
        std::fs::create_dir_all(&data)?;
        let store = FileStore::open_with(&data, FileStoreConfig::default()).context("open FileStore")?;
        let mut ccfg = ChainConfig::default();
        let inv = if replay.is_some() && cfg.invariants == Invariants::EveryN(100) { Invariants::Every } else { cfg.invariants.clone() };
        ccfg.invariants = inv.to_mode();
        let mut chain = HlChain::new_hl(genesis.clone(), ccfg);
        let genesis_root = chain.root_at(0).unwrap_or(Hash32::ZERO);
        if let Some(s) = store.latest_snapshot() {
            let root = chain.restore(&s).map_err(|e| anyhow!("restore snapshot {}: {e}", s.height))?;
            anyhow::ensure!(root == s.state_root, "snapshot {} root mismatch", s.height);
            info!(height = s.height, "restored from snapshot");
        }
        let exec = ExecInfo {
            height: chain.executed_height(),
            root: chain.current_root(),
            genesis_root,
            roots: [(chain.executed_height(), chain.current_root())].into_iter().collect(),
        };
        let sig_cache = chain.sig_cache();

        // ---- p2p
        let mut ncfg = NetConfig {
            chain_id: genesis.chain_id,
            secret_key: wallet.secret_bytes(),
            validators: genesis.validators.clone(),
            ..Default::default()
        };
        ncfg.listen_addr = cfg.p2p_listen.clone();
        let (p2p, inbound) = P2p::start(ncfg).map_err(|e| anyhow!("p2p start: {e:?}"))?;

        // ---- mempool (checker filled in once Shared exists)
        type Checker = dyn Fn(&Tx) -> Result<Address, AdmissionError> + Send + Sync;
        let checker_slot: Arc<RwLock<Option<Arc<Checker>>>> = Arc::new(RwLock::new(None));
        let cs = checker_slot.clone();
        let checker: mempool::AdmissionChecker = Arc::new(move |tx: &Tx| {
            let f = cs.read().clone();
            match f {
                Some(f) => f(tx).map(|_| ()),
                None => Err(AdmissionError::Other("node starting".into())),
            }
        });
        let pool = Pool::new(cfg.mempool.clone(), PoolDeps::new(checker).with_network(p2p.clone() as Arc<dyn Network>));

        let shared = Arc::new(Shared {
            verifier: hlwire::HlVerifier::new(genesis.is_mainnet),
            genesis: genesis.clone(),
            wallet: wallet.clone(),
            chain: RwLock::new(chain),
            sig_cache,
            pool: pool.clone(),
            p2p: p2p.clone(),
            exec: Mutex::new(exec),
            inflight: InFlight::default(),
            callbacks: RwLock::new(Vec::new()),
            driver_status: Mutex::new(None),
            last_commit_ms: AtomicU64::new(0),
            vnonce: Mutex::new(0),
        });
        {
            let s = shared.clone();
            *checker_slot.write() = Some(Arc::new(move |tx: &Tx| s.check_tx(tx)));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();

        // ---- peer tx gossip -> mempool (2 threads: recovery is the cost)
        for i in 0..2 {
            let rx = inbound.txs.clone();
            let pool = pool.clone();
            threads.push(
                std::thread::Builder::new()
                    .name(format!("peer-txs-{i}"))
                    .spawn(move || {
                        while let Ok((from, msg)) = rx.recv() {
                            if let NetMsg::Txs(txs) = msg {
                                pool.handle_peer_txs(from, txs);
                            }
                        }
                    })?,
            );
        }

        // ---- consensus inbound adapter (records in-flight payloads)
        let (cons_tx, cons_rx) = crossbeam_channel::unbounded::<(Address, ConsensusMsg)>();
        {
            let rx = inbound.consensus.clone();
            let s = shared.clone();
            threads.push(std::thread::Builder::new().name("cons-inbound".into()).spawn(move || {
                while let Ok((from, msg)) = rx.recv() {
                    let NetMsg::Consensus(m) = msg else { continue };
                    if let ConsensusMsg::Proposal { block, .. } = &m {
                        if !block.payload.is_empty() {
                            s.inflight.add(block.header.height, block.payload.iter().map(|t| t.hash()));
                        }
                    }
                    if cons_tx.send((from, m)).is_err() {
                        return;
                    }
                }
            })?);
        }

        // ---- executor
        let (exec_tx, exec_rx) = crossbeam_channel::unbounded::<ExecMsg>();
        let roots_log = HeightLog::open(&cfg.home.join("roots.log")).context("open roots.log")?;
        let mut commits_log = HeightLog::open(&cfg.home.join("commits.log")).context("open commits.log")?;

        // ---- consensus driver
        let mut core_cfg = CoreConfig::new(genesis.chain_id, genesis.genesis_time_ms, genesis.validators.clone(), me);
        core_cfg.min_block_interval_ms = cfg.min_block_ms;
        core_cfg.max_block_txs = genesis.params.max_block_txs as usize;
        core_cfg.max_block_bytes = genesis.params.max_block_bytes as usize;
        let sk = secp256k1::SecretKey::from_byte_array(&wallet.secret_bytes()).map_err(|e| anyhow!("{e}"))?;
        let env = NodeEnv { shared: shared.clone(), store, sk, replay };
        let s2 = shared.clone();
        let sink: Box<dyn FnMut(ConsensusOutput) + Send> = Box::new(move |o| match o {
            ConsensusOutput::Commit(cb) => {
                let h = &cb.block.header;
                let id = cb.block.id();
                match commits_log.get(h.height) {
                    Some(prev) if prev != hex32(&id) => {
                        error!(height = h.height, previous = prev, now = %hex32(&id), "conflicting commit vs commits.log")
                    }
                    _ => {
                        if let Err(e) = commits_log.append(h.height, &commit_line(h.height, &id, h.timestamp_ms)) {
                            error!("commits.log write failed: {e}");
                        }
                    }
                }
                s2.last_commit_ms.store(now_ms(), Ordering::Relaxed);
                s2.inflight.set_committed(h.height);
                let _ = exec_tx.send(ExecMsg::Commit(cb));
            }
            ConsensusOutput::InstallSnapshot(s) => {
                let _ = exec_tx.send(ExecMsg::Install(s));
            }
            _ => {}
        });
        let driver = Driver::spawn(core_cfg, env, p2p.clone() as Arc<dyn Network>, cons_rx, sink);
        *shared.driver_status.lock() = Some(driver.status());
        {
            let ex = Executor {
                shared: shared.clone(),
                roots_log,
                driver: driver.sender(),
                snapshot_interval: cfg.snapshot_interval,
                replay_end,
                divergences: 0,
            };
            threads.push(std::thread::Builder::new().name("executor".into()).spawn(move || ex.run(exec_rx))?);
        }

        // ---- oracle feeder
        if let Some(mut feed) = make_feed(&cfg.oracle_feed, &genesis, my_index) {
            let s = shared.clone();
            let st = stop.clone();
            let interval = cfg.oracle_interval_ms.max(50);
            threads.push(std::thread::Builder::new().name("oracle".into()).spawn(move || {
                while !st.load(Ordering::Relaxed) {
                    let prices = feed.prices(now_ms());
                    if !prices.is_empty() {
                        if let Err(e) = s.submit_validator_action(ValidatorAction::OraclePrices { prices }) {
                            warn!("oracle submission rejected: {e}");
                        }
                    }
                    let mut slept = 0;
                    while slept < interval && !st.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(50.min(interval)));
                        slept += 50;
                    }
                }
            })?);
        }

        // ---- tokio: API + bridge watcher
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("api").enable_all().build()?;
        let backend = Arc::new(Backend(shared.clone()));
        let addr: std::net::SocketAddr = cfg.api_listen.parse().with_context(|| format!("bad api listen {}", cfg.api_listen))?;
        let listener = runtime.block_on(tokio::net::TcpListener::bind(addr)).with_context(|| format!("bind api {addr}"))?;
        {
            let b: Arc<dyn ApiBackend> = backend.clone();
            runtime.spawn(async move {
                let shared = api::Shared::start(b, api::ApiConfig::default());
                if let Err(e) = api::serve_listener(shared, listener).await {
                    error!("api server failed: {e}");
                    std::process::exit(3);
                }
            });
        }
        if let Some(rpc) = &cfg.bridge_rpc {
            let mut wc = relayer::WatcherConfig::new(rpc.clone());
            wc.confirmations = cfg.bridge_confirmations;
            let host: Arc<dyn BridgeHost> = backend.clone();
            let _g = runtime.enter();
            relayer::DepositWatcher::new(host, wc).spawn();
            info!(rpc = %rpc, "bridge watcher enabled");
        }
        info!(api = %addr, "node started");
        Ok(Node { shared, driver: Some(driver), stop, threads, runtime: Some(runtime) })
    }

    pub fn shutdown(mut self) {
        info!("shutting down");
        self.stop.store(true, Ordering::Relaxed);
        if let Some(d) = self.driver.take() {
            d.shutdown();
        }
        self.shared.p2p.shutdown();
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(Duration::from_secs(2));
        }
        // the executor and inbound threads exit when their channels close; don't wait on them.
        let oracle: Vec<_> = self.threads.drain(..).filter(|t| t.thread().name() == Some("oracle")).collect();
        for t in oracle {
            let _ = t.join();
        }
    }
}

fn make_feed(spec: &FeedSpec, g: &Genesis, my_index: usize) -> Option<Box<dyn PriceFeed>> {
    match spec {
        FeedSpec::None => None,
        FeedSpec::Fixed => {
            let px: Vec<(AssetId, Px)> = g.markets.iter().map(|m| (m.asset, m.initial_px)).collect();
            Some(Box::new(oracle::feeds::FixedFeed::new(&px)))
        }
        FeedSpec::File(p) => Some(Box::new(oracle::feeds::FilePriceFeed::new(p.clone()))),
        FeedSpec::Synthetic { seed, skew_bps } => {
            let mut f = oracle::feeds::SyntheticFeed::new(
                &g.markets,
                oracle::feeds::SynthParams::default(),
                g.genesis_time_ms,
                *seed,
                my_index as u64 + 1,
            );
            if *skew_bps != 0 {
                // 1 bp = 1e-4 = 1e8 Rate units
                f = f.with_skew(oracle::feeds::Skew::Fixed(Rate::from_raw(*skew_bps as i128 * 100_000_000)));
            }
            Some(Box::new(f))
        }
    }
}
