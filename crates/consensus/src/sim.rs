//! Deterministic in-process network simulator for [`Core`] (used by the
//! test-suite; public so other harnesses can reuse it).
//!
//! * virtual clock + seeded ChaCha RNG: a run is a pure function of
//!   `SimConfig` and the sequence of control calls,
//! * per-message drop / delay / duplication (reordering falls out of random
//!   delays), partitions (checked at delivery time),
//! * crash / restart with the persisted store (MemStore kept across the crash,
//!   or a FileStore re-opened from disk),
//! * Byzantine behaviours (equivocating leader, double voting, replay of
//!   stale proposals/timeouts with old QCs, lying status),
//! * a fake executor (`root = blake3(prev_root || block_id)`), optionally
//!   diverging, with periodic snapshots,
//! * an online safety checker over honest nodes.

use crate::config::CoreConfig;
use crate::core::Core;
use crate::crypto;
use crate::store::{FileStore, FileStoreConfig, MemStore};
use interfaces::*;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

pub const SIM_GENESIS_TIME: u64 = 1_700_000_000_000;
pub const SIM_CHAIN_ID: u64 = 7777;

/// Deterministic fake signature scheme: `r = blake3(secret(addr) || digest)`.
#[derive(Clone, Default)]
pub struct FakeKeys;

impl FakeKeys {
    fn secret(a: &Address) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"sim-secret");
        h.update(&a.0);
        *h.finalize().as_bytes()
    }
    pub fn sign(a: &Address, digest: &[u8; 32]) -> Sig65 {
        let mut h = blake3::Hasher::new();
        h.update(&Self::secret(a));
        h.update(digest);
        Sig65 { r: *h.finalize().as_bytes(), s: [0; 32], v: 27 }
    }
    pub fn verify(digest: &[u8; 32], sig: &Sig65, signer: &Address) -> bool {
        Self::sign(signer, digest) == *sig
    }
}

#[derive(Clone, Debug)]
pub enum StoreKind {
    Mem { retain_heights: u64 },
    /// One sub-directory per node under this base.
    File { base: PathBuf, retain_heights: u64 },
}

#[derive(Clone, Debug)]
pub struct SimConfig {
    pub n: usize,
    pub seed: u64,
    pub min_delay_ms: u64,
    pub max_delay_ms: u64,
    pub drop_ppm: u32,
    pub dup_ppm: u32,
    pub txs_per_block: usize,
    pub tx_bytes: usize,
    pub exec_delay_ms: u64,
    pub snapshot_every: u64,
    pub store: StoreKind,
    pub min_block_interval_ms: u64,
    /// Applied to every node's CoreConfig.
    pub tweak: Option<fn(&mut CoreConfig)>,
    /// Stakes (default: all 1).
    pub stakes: Option<Vec<u64>>,
}

impl SimConfig {
    pub fn new(n: usize, seed: u64) -> Self {
        SimConfig {
            n,
            seed,
            min_delay_ms: 5,
            max_delay_ms: 50,
            drop_ppm: 0,
            dup_ppm: 0,
            txs_per_block: 3,
            tx_bytes: 64,
            exec_delay_ms: 10,
            snapshot_every: 50,
            store: StoreKind::Mem { retain_heights: 0 },
            min_block_interval_ms: 200,
            tweak: None,
            stakes: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Byzantine {
    /// As leader, send different blocks to different halves of the network.
    pub equivocate: bool,
    /// Vote for an extra (conflicting / fabricated) block in each view.
    pub double_vote: bool,
    /// Periodically re-send stale proposals and timeouts (old QCs/TCs).
    pub stale_replay: bool,
    /// Advertise an inflated committed height in Status.
    pub lie_status: bool,
}

#[derive(Default)]
pub struct FakeExec {
    pub height: Height,
    pub root: Hash32,
    pub roots: BTreeMap<Height, Hash32>,
    pub diverge: bool,
    queue: VecDeque<ExecItem>,
}

enum ExecItem {
    Block(Height, BlockId),
    Snap(Snapshot),
}

pub struct SimNode {
    pub addr: Address,
    pub core: Option<Core>,
    pub store: Box<dyn BlockStore>,
    pub exec: FakeExec,
    pub byz: Option<Byzantine>,
    /// Every commit ever emitted by this node (across restarts).
    pub commits: BTreeMap<Height, BlockId>,
    /// Height of the last Commit/InstallSnapshot emitted by the current
    /// incarnation (strict-order check).
    out_tail: Height,
    tx_counter: u64,
    wakes: BTreeSet<u64>,
    byz_history: Vec<ConsensusMsg>,
    pub restarts: u32,
}

enum Event {
    Deliver { to: usize, from: usize, msg: ConsensusMsg },
    Wake { node: usize },
    Exec { node: usize },
}

pub type ReplayLog = Arc<BTreeMap<Height, (TimestampMs, Vec<Tx>)>>;

pub struct Sim {
    pub cfg: SimConfig,
    pub now: u64,
    pub nodes: Vec<SimNode>,
    pub validators: ValidatorSet,
    events: BTreeMap<(u64, u64), Event>,
    seq: u64,
    rng: ChaCha8Rng,
    pub partition: Vec<u32>,
    pub drop_ppm: u32,
    pub dup_ppm: u32,
    pub min_delay_ms: u64,
    pub max_delay_ms: u64,
    pub violations: Vec<String>,
    pub global_commits: BTreeMap<Height, BlockId>,
    pub global_roots: BTreeMap<Height, Hash32>,
    /// Virtual time of each new max honest committed height.
    pub commit_times: Vec<(u64, Height)>,
    pub replay: Option<ReplayLog>,
    pub messages_delivered: u64,
    /// (honest node, view) -> block voted (across restarts).
    votes_cast: BTreeMap<(usize, View), BlockId>,
}

struct Env<'a> {
    now: u64,
    me: Address,
    store: &'a mut dyn BlockStore,
    exec: &'a FakeExec,
    tx_counter: &'a mut u64,
    txs_per_block: usize,
    tx_bytes: usize,
    replay: Option<&'a ReplayLog>,
}

impl ConsensusEnv for Env<'_> {
    fn now_ms(&self) -> u64 {
        self.now
    }
    fn payload(&mut self, max_txs: usize, _max_bytes: usize) -> Vec<Tx> {
        let n = self.txs_per_block.min(max_txs);
        (0..n)
            .map(|_| {
                *self.tx_counter += 1;
                let mut bytes = vec![0u8; self.tx_bytes.max(28)];
                bytes[..20].copy_from_slice(&self.me.0);
                bytes[20..28].copy_from_slice(&self.tx_counter.to_le_bytes());
                Tx::User(UserTx {
                    action_bytes: bytes,
                    nonce: *self.tx_counter,
                    signature: Sig65::default(),
                    vault_address: None,
                    expires_after: None,
                })
            })
            .collect()
    }
    fn exec_state(&self) -> (Height, Hash32) {
        (self.exec.height, self.exec.root)
    }
    fn root_at(&self, h: Height) -> Option<Hash32> {
        if h == 0 {
            return Some(Hash32::ZERO);
        }
        self.exec.roots.get(&h).copied()
    }
    fn snapshot(&self) -> Option<Snapshot> {
        self.store.latest_snapshot()
    }
    fn proposal_override(&mut self, height: Height) -> Option<(TimestampMs, Vec<Tx>)> {
        self.replay.and_then(|r| r.get(&height).cloned())
    }
    fn sign(&self, digest: &[u8; 32]) -> Sig65 {
        FakeKeys::sign(&self.me, digest)
    }
    fn verify(&self, digest: &[u8; 32], sig: &Sig65, signer: &Address) -> bool {
        FakeKeys::verify(digest, sig, signer)
    }
    fn store(&mut self) -> &mut dyn BlockStore {
        self.store
    }
}

pub fn sim_address(i: usize) -> Address {
    let mut a = [0u8; 20];
    a[0] = 0xA0;
    a[19] = i as u8 + 1;
    Address(a)
}

fn exec_root(prev: &Hash32, id: &BlockId, diverge: bool) -> Hash32 {
    let mut h = blake3::Hasher::new();
    h.update(&prev.0);
    h.update(&id.0);
    if diverge {
        h.update(b"evil");
    }
    Hash32(*h.finalize().as_bytes())
}

impl Sim {
    pub fn new(cfg: SimConfig) -> Sim {
        let stakes = cfg.stakes.clone().unwrap_or_else(|| vec![1; cfg.n]);
        let validators = ValidatorSet {
            validators: (0..cfg.n)
                .map(|i| ValidatorInfo { address: sim_address(i), stake: stakes[i], p2p_addr: format!("sim:{i}") })
                .collect(),
        };
        let mut sim = Sim {
            now: SIM_GENESIS_TIME,
            nodes: vec![],
            validators,
            events: BTreeMap::new(),
            seq: 0,
            rng: ChaCha8Rng::seed_from_u64(cfg.seed),
            partition: vec![0; cfg.n],
            drop_ppm: cfg.drop_ppm,
            dup_ppm: cfg.dup_ppm,
            min_delay_ms: cfg.min_delay_ms,
            max_delay_ms: cfg.max_delay_ms,
            violations: vec![],
            global_commits: BTreeMap::new(),
            global_roots: BTreeMap::new(),
            commit_times: vec![],
            replay: None,
            messages_delivered: 0,
            votes_cast: BTreeMap::new(),
            cfg,
        };
        for i in 0..sim.cfg.n {
            let store = sim.open_store(i);
            sim.nodes.push(SimNode {
                addr: sim_address(i),
                core: None,
                store,
                exec: FakeExec::default(),
                byz: None,
                commits: BTreeMap::new(),
                out_tail: 0,
                tx_counter: 0,
                wakes: BTreeSet::new(),
                byz_history: vec![],
                restarts: 0,
            });
        }
        for i in 0..sim.cfg.n {
            sim.boot(i);
        }
        sim
    }

    fn open_store(&self, i: usize) -> Box<dyn BlockStore> {
        match &self.cfg.store {
            StoreKind::Mem { retain_heights } => Box::new(MemStore::with_retention(*retain_heights)),
            StoreKind::File { base, retain_heights } => {
                let cfg = FileStoreConfig { retain_heights: *retain_heights, segment_bytes: 1 << 20, fsync_commits: false, keep_snapshots: 2 };
                Box::new(FileStore::open_with(base.join(format!("node{i}")), cfg).expect("open filestore"))
            }
        }
    }

    pub fn core_config(&self, i: usize) -> CoreConfig {
        let mut c = CoreConfig::new(SIM_CHAIN_ID, SIM_GENESIS_TIME, self.validators.clone(), sim_address(i));
        c.min_block_interval_ms = self.cfg.min_block_interval_ms;
        c.snapshot_interval = if self.cfg.snapshot_every == 0 { u64::MAX / 2 } else { self.cfg.snapshot_every };
        if let Some(t) = self.cfg.tweak {
            t(&mut c);
        }
        c
    }

    fn boot(&mut self, i: usize) {
        let cfg = self.core_config(i);
        let now = self.now;
        let replay = self.replay.clone();
        let n = &mut self.nodes[i];
        // executor restarts from its latest snapshot (state is not persisted otherwise)
        n.exec = FakeExec { diverge: n.exec.diverge, ..FakeExec::default() };
        if let Some(s) = n.store.latest_snapshot() {
            let root = Hash32::try_from_slice_root(&s.data);
            n.exec.height = s.height;
            n.exec.root = root;
            n.exec.roots.insert(s.height, root);
        }
        n.out_tail = n.exec.height;
        n.wakes.clear();
        let mut env = Env {
            now,
            me: n.addr,
            store: n.store.as_mut(),
            exec: &n.exec,
            tx_counter: &mut n.tx_counter,
            txs_per_block: self.cfg.txs_per_block,
            tx_bytes: self.cfg.tx_bytes,
            replay: replay.as_ref(),
        };
        n.core = Some(Core::new(cfg, &mut env));
        self.push(now, Event::Wake { node: i });
    }

    fn push(&mut self, at: u64, ev: Event) {
        self.seq += 1;
        self.events.insert((at, self.seq), ev);
    }

    // ----------------------------------------------------------- control --

    pub fn crash(&mut self, i: usize) {
        self.nodes[i].core = None;
        if let StoreKind::File { .. } = self.cfg.store {
            // drop file handles (kill -9: nothing is flushed beyond what was written)
            self.nodes[i].store = Box::new(MemStore::new());
        }
    }

    pub fn restart(&mut self, i: usize) {
        if self.nodes[i].core.is_some() {
            return;
        }
        if let StoreKind::File { .. } = self.cfg.store {
            self.nodes[i].store = self.open_store(i);
        }
        self.nodes[i].restarts += 1;
        self.boot(i);
    }

    pub fn is_up(&self, i: usize) -> bool {
        self.nodes[i].core.is_some()
    }

    /// Nodes in different groups cannot communicate. Unlisted nodes are
    /// isolated.
    pub fn set_partition(&mut self, groups: &[Vec<usize>]) {
        let mut p: Vec<u32> = (0..self.cfg.n).map(|i| 1000 + i as u32).collect();
        for (g, members) in groups.iter().enumerate() {
            for &m in members {
                p[m] = g as u32;
            }
        }
        self.partition = p;
    }

    pub fn heal(&mut self) {
        self.partition = vec![0; self.cfg.n];
    }

    pub fn set_byzantine(&mut self, i: usize, b: Byzantine) {
        self.nodes[i].byz = Some(b);
    }

    pub fn set_diverging_executor(&mut self, i: usize) {
        self.nodes[i].exec.diverge = true;
    }

    pub fn is_honest(&self, i: usize) -> bool {
        self.nodes[i].byz.is_none()
    }

    // ------------------------------------------------------------ running --

    pub fn run_for(&mut self, ms: u64) {
        let t = self.now + ms;
        self.run_until(t);
    }

    pub fn run_until(&mut self, t: u64) {
        while let Some((&(at, seq), _)) = self.events.iter().next() {
            if at > t {
                break;
            }
            let ev = self.events.remove(&(at, seq)).unwrap();
            self.now = at;
            self.process(ev);
        }
        self.now = t;
    }

    fn process(&mut self, ev: Event) {
        match ev {
            Event::Deliver { to, from, msg } => {
                if self.partition[to] != self.partition[from] || self.nodes[to].core.is_none() {
                    return;
                }
                self.messages_delivered += 1;
                if let Some(b) = &self.nodes[to].byz {
                    if b.stale_replay && matches!(msg, ConsensusMsg::Proposal { .. } | ConsensusMsg::Timeout(_)) {
                        let h = &mut self.nodes[to].byz_history;
                        if h.len() < 64 {
                            h.push(msg.clone());
                        } else {
                            let k = self.rng.gen_range(0..64);
                            h[k] = msg.clone();
                        }
                    }
                }
                let from_addr = self.nodes[from].addr;
                self.step(to, ConsensusInput::Message { from: from_addr, msg });
            }
            Event::Wake { node } => {
                self.nodes[node].wakes.remove(&self.now);
                if self.nodes[node].core.is_none() {
                    return;
                }
                self.byz_wake(node);
                self.step(node, ConsensusInput::Tick);
            }
            Event::Exec { node } => self.exec_one(node),
        }
    }

    fn step(&mut self, i: usize, input: ConsensusInput) {
        let now = self.now;
        let replay = self.replay.clone();
        let n = &mut self.nodes[i];
        let Some(core) = n.core.as_mut() else { return };
        let _span = tracing::debug_span!("node", i, t = now - SIM_GENESIS_TIME).entered();
        let mut env = Env {
            now,
            me: n.addr,
            store: n.store.as_mut(),
            exec: &n.exec,
            tx_counter: &mut n.tx_counter,
            txs_per_block: self.cfg.txs_per_block,
            tx_bytes: self.cfg.tx_bytes,
            replay: replay.as_ref(),
        };
        let outs = core.handle(input, &mut env);
        self.route(i, outs);
    }

    /// Invariants on every outgoing vote/timeout of an honest node: the
    /// safety state covering it is already persisted, and no two votes in
    /// one view.
    fn check_outgoing(&mut self, i: usize, msg: &ConsensusMsg) {
        if !self.is_honest(i) {
            return;
        }
        match msg {
            ConsensusMsg::Vote(v) => {
                let st = self.nodes[i].store.load_safety();
                if st.as_ref().is_none_or(|s| s.last_voted_view < v.data.view) {
                    self.violations.push(format!("node {i}: vote for view {} sent before safety persisted", v.data.view));
                }
                if let Some(prev) = self.votes_cast.insert((i, v.data.view), v.data.block_id) {
                    if prev != v.data.block_id {
                        self.violations.push(format!("node {i}: double vote in view {}", v.data.view));
                    }
                }
            }
            ConsensusMsg::Timeout(t) => {
                let st = self.nodes[i].store.load_safety();
                if st.as_ref().is_none_or(|s| s.last_timeout_view < t.data.view || s.high_qc.data.view < t.data.high_qc_view) {
                    self.violations.push(format!("node {i}: timeout for view {} sent before safety persisted", t.data.view));
                }
            }
            _ => {}
        }
    }

    fn route(&mut self, i: usize, outs: Vec<ConsensusOutput>) {
        for o in &outs {
            match o {
                ConsensusOutput::Send { msg, .. } | ConsensusOutput::Broadcast(msg) => self.check_outgoing(i, msg),
                _ => {}
            }
        }
        for o in outs {
            match o {
                ConsensusOutput::Send { to, msg } => {
                    if let Some(j) = self.index_of(&to) {
                        self.send_byz(i, j, msg);
                    }
                }
                ConsensusOutput::Broadcast(msg) => {
                    if let Some(ConsensusMsg::Proposal { .. }) = Some(&msg) {
                        if self.nodes[i].byz.as_ref().is_some_and(|b| b.equivocate) {
                            self.equivocate(i, msg);
                            continue;
                        }
                    }
                    let msg = self.lie(i, msg);
                    for j in 0..self.cfg.n {
                        if j != i {
                            self.net_send(i, j, msg.clone());
                        }
                    }
                }
                ConsensusOutput::SetTimer { at_ms } => {
                    let at = at_ms.max(self.now + 1);
                    if self.nodes[i].wakes.insert(at) {
                        self.push(at, Event::Wake { node: i });
                    }
                }
                ConsensusOutput::Commit(cb) => self.on_commit(i, cb),
                ConsensusOutput::InstallSnapshot(s) => {
                    let n = &mut self.nodes[i];
                    if s.height <= n.out_tail {
                        self.violations.push(format!("node {i}: InstallSnapshot({}) not above tail {}", s.height, n.out_tail));
                    }
                    n.out_tail = s.height;
                    n.exec.queue.push_back(ExecItem::Snap(s));
                    let at = self.now + self.cfg.exec_delay_ms;
                    self.push(at, Event::Exec { node: i });
                }
            }
        }
    }

    fn index_of(&self, a: &Address) -> Option<usize> {
        self.nodes.iter().position(|n| &n.addr == a)
    }

    fn net_send(&mut self, from: usize, to: usize, msg: ConsensusMsg) {
        if self.drop_ppm > 0 && self.rng.gen_range(0..1_000_000) < self.drop_ppm {
            return;
        }
        let d = self.rng.gen_range(self.min_delay_ms..=self.max_delay_ms);
        if self.dup_ppm > 0 && self.rng.gen_range(0..1_000_000) < self.dup_ppm {
            let d2 = self.rng.gen_range(self.min_delay_ms..=self.max_delay_ms * 2);
            self.push(self.now + d2, Event::Deliver { to, from, msg: msg.clone() });
        }
        self.push(self.now + d, Event::Deliver { to, from, msg });
    }

    fn on_commit(&mut self, i: usize, cb: CommittedBlock) {
        let h = cb.block.header.height;
        let id = cb.block.id();
        let honest = self.is_honest(i);
        let n = &mut self.nodes[i];
        if h != n.out_tail + 1 {
            self.violations.push(format!("node {i}: commit {h} out of order (tail {})", n.out_tail));
        }
        n.out_tail = h;
        if let Some(prev) = n.commits.get(&h) {
            if *prev != id {
                self.violations.push(format!("node {i}: re-committed height {h} with a different block"));
            }
        }
        n.commits.insert(h, id);
        n.exec.queue.push_back(ExecItem::Block(h, id));
        let at = self.now + self.cfg.exec_delay_ms;
        self.push(at, Event::Exec { node: i });
        if honest {
            match self.global_commits.get(&h) {
                Some(g) if *g != id => {
                    self.violations.push(format!("SAFETY: node {i} committed {id} at height {h}, others committed {g}"));
                }
                Some(_) => {}
                None => {
                    self.global_commits.insert(h, id);
                    self.commit_times.push((self.now, h));
                }
            }
        }
    }

    fn exec_one(&mut self, i: usize) {
        if self.nodes[i].core.is_none() {
            return;
        }
        let n = &mut self.nodes[i];
        let Some(item) = n.exec.queue.pop_front() else { return };
        let (h, root) = match item {
            ExecItem::Block(h, id) => {
                if h != n.exec.height + 1 {
                    // stale queue entry (e.g. already covered by a snapshot)
                    if h <= n.exec.height {
                        return;
                    }
                    self.violations.push(format!("node {i}: executor gap: got {h} after {}", n.exec.height));
                    return;
                }
                let r = exec_root(&n.exec.root, &id, n.exec.diverge);
                (h, r)
            }
            ExecItem::Snap(s) => {
                let r = Hash32::try_from_slice_root(&s.data);
                if r != s.state_root {
                    self.violations.push(format!("node {i}: bad snapshot data"));
                }
                (s.height, r)
            }
        };
        n.exec.height = h;
        n.exec.root = root;
        n.exec.roots.insert(h, root);
        let lo = h.saturating_sub(20_000);
        if n.exec.roots.keys().next().is_some_and(|k| *k < lo) {
            n.exec.roots = n.exec.roots.split_off(&lo);
        }
        if self.cfg.snapshot_every > 0 && h % self.cfg.snapshot_every == 0 {
            let s = Snapshot { height: h, state_root: root, data: root.0.to_vec() };
            let _ = n.store.save_snapshot(&s);
        }
        let honest = n.byz.is_none() && !n.exec.diverge;
        if honest {
            match self.global_roots.get(&h) {
                Some(g) if *g != root => self.violations.push(format!("node {i}: root mismatch at {h}")),
                Some(_) => {}
                None => {
                    self.global_roots.insert(h, root);
                }
            }
        }
        self.step(i, ConsensusInput::Executed { height: h, root });
    }

    // --------------------------------------------------------- byzantine --

    fn lie(&mut self, i: usize, msg: ConsensusMsg) -> ConsensusMsg {
        match (&self.nodes[i].byz, msg) {
            (Some(b), ConsensusMsg::Status { committed_height, high_qc_view }) if b.lie_status => {
                ConsensusMsg::Status { committed_height: committed_height + 1000, high_qc_view }
            }
            (_, m) => m,
        }
    }

    fn equivocate(&mut self, i: usize, msg: ConsensusMsg) {
        let ConsensusMsg::Proposal { block, tc } = msg else { return };
        let mut b2 = block.clone();
        b2.payload.push(Tx::User(UserTx {
            action_bytes: b"equivocation".to_vec(),
            nonce: self.now,
            signature: Sig65::default(),
            vault_address: None,
            expires_after: None,
        }));
        b2.header.payload_hash = crypto::payload_hash(&b2.payload);
        let double_vote = self.nodes[i].byz.as_ref().is_some_and(|b| b.double_vote);
        if double_vote {
            // also vote for the second block
            let kind = crypto::VoteKind::of_header(&b2.header);
            let data = VoteData { chain_id: SIM_CHAIN_ID, block_id: b2.id(), view: b2.header.view, height: b2.header.height };
            let me = self.nodes[i].addr;
            let vote = Vote { signature: FakeKeys::sign(&me, &crypto::vote_digest(&data, kind)), data, voter: me };
            let next = self.validators.leader(b2.header.view + 1);
            if let Some(j) = self.index_of(&next) {
                if j != i {
                    self.net_send(i, j, ConsensusMsg::Vote(vote));
                }
            }
        }
        for j in 0..self.cfg.n {
            if j == i {
                continue;
            }
            let m = if j % 2 == 0 {
                ConsensusMsg::Proposal { block: block.clone(), tc: tc.clone() }
            } else {
                ConsensusMsg::Proposal { block: b2.clone(), tc: tc.clone() }
            };
            self.net_send(i, j, m);
        }
    }

    fn send_byz(&mut self, i: usize, j: usize, msg: ConsensusMsg) {
        if let (Some(b), ConsensusMsg::Vote(v)) = (&self.nodes[i].byz, &msg) {
            if b.double_vote {
                let mut data = v.data.clone();
                let mut fake = [0u8; 32];
                self.rng.fill(&mut fake);
                data.block_id = Hash32(fake);
                let me = self.nodes[i].addr;
                for kind in [crypto::VoteKind::Direct, crypto::VoteKind::AfterTimeout] {
                    let vote = Vote { signature: FakeKeys::sign(&me, &crypto::vote_digest(&data, kind)), data: data.clone(), voter: me };
                    // conflicting vote first, sometimes
                    if self.rng.gen_range(0..2) == 0 {
                        self.net_send(i, j, ConsensusMsg::Vote(vote));
                    }
                }
            }
        }
        self.net_send(i, j, msg);
    }

    fn byz_wake(&mut self, i: usize) {
        let Some(b) = self.nodes[i].byz.clone() else { return };
        if b.stale_replay && !self.nodes[i].byz_history.is_empty() && self.rng.gen_range(0..4) == 0 {
            let k = self.rng.gen_range(0..self.nodes[i].byz_history.len());
            let m = self.nodes[i].byz_history[k].clone();
            for j in 0..self.cfg.n {
                if j != i {
                    self.net_send(i, j, m.clone());
                }
            }
        }
    }

    // ----------------------------------------------------------- queries --

    pub fn core(&self, i: usize) -> Option<&Core> {
        self.nodes[i].core.as_ref()
    }

    pub fn committed_height(&self, i: usize) -> Height {
        self.nodes[i].core.as_ref().map(interfaces::ConsensusCore::committed_height).unwrap_or(0)
    }

    pub fn honest_up(&self) -> Vec<usize> {
        (0..self.cfg.n).filter(|i| self.is_honest(*i) && self.is_up(*i)).collect()
    }

    pub fn max_honest_committed(&self) -> Height {
        self.global_commits.keys().next_back().copied().unwrap_or(0)
    }

    pub fn min_honest_committed(&self) -> Height {
        self.honest_up().iter().map(|i| self.committed_height(*i)).min().unwrap_or(0)
    }

    pub fn total_divergences(&self) -> u64 {
        self.nodes.iter().filter_map(|n| n.core.as_ref()).map(|c| c.stats().divergences).sum()
    }

    pub fn total_safety_violations(&self) -> u64 {
        self.nodes.iter().filter_map(|n| n.core.as_ref()).map(|c| c.stats().safety_violations).sum()
    }

    /// Full safety check over honest nodes: no recorded violations, and each
    /// honest node's commit log agrees with the global log.
    pub fn check_safety(&self) -> Result<(), String> {
        if !self.violations.is_empty() {
            return Err(self.violations.join("\n"));
        }
        for (i, n) in self.nodes.iter().enumerate() {
            if !self.is_honest(i) {
                continue;
            }
            for (h, id) in &n.commits {
                if self.global_commits.get(h) != Some(id) {
                    return Err(format!("node {i} height {h} disagrees with global log"));
                }
            }
            if let Some(c) = &n.core {
                if c.stats().safety_violations > 0 {
                    return Err(format!("node {i} reported a safety violation"));
                }
            }
        }
        Ok(())
    }

    /// Wait (virtual time, up to `max_ms`) until the max honest committed
    /// height reaches `target`; returns elapsed ms or None.
    pub fn wait_for_height(&mut self, target: Height, max_ms: u64) -> Option<u64> {
        let start = self.now;
        while self.now < start + max_ms {
            if self.max_honest_committed() >= target {
                return Some(self.now - start);
            }
            self.run_for(10);
        }
        if self.max_honest_committed() >= target {
            Some(self.now - start)
        } else {
            None
        }
    }

    /// Wait until every listed node has committed at least `target`.
    pub fn wait_all(&mut self, nodes: &[usize], target: Height, max_ms: u64) -> Option<u64> {
        let start = self.now;
        loop {
            if nodes.iter().all(|i| self.committed_height(*i) >= target) {
                return Some(self.now - start);
            }
            if self.now >= start + max_ms {
                return None;
            }
            self.run_for(10);
        }
    }
}

trait RootFromData {
    fn try_from_slice_root(d: &[u8]) -> Hash32;
}
impl RootFromData for Hash32 {
    fn try_from_slice_root(d: &[u8]) -> Hash32 {
        let mut r = [0u8; 32];
        if d.len() == 32 {
            r.copy_from_slice(d);
        }
        Hash32(r)
    }
}
