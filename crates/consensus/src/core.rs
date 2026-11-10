//! The sans-IO consensus state machine. See README.md for the protocol.

use crate::config::CoreConfig;
use crate::crypto::{self, CertCtx, VoteKind};
use interfaces::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Counters exposed for tests, metrics and the chaos harness.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CoreStats {
    /// `STATE DIVERGENCE` events (local root != certified/claimed root).
    pub divergences: u64,
    /// A QC chain tried to commit a block conflicting with our committed
    /// chain (impossible with <= f Byzantine stake).
    pub safety_violations: u64,
    pub proposals: u64,
    pub votes: u64,
    pub timeouts: u64,
    pub qcs_formed: u64,
    pub tcs_formed: u64,
    pub commits: u64,
    pub synced_blocks: u64,
    pub snapshots_installed: u64,
    /// Validators seen voting for two different blocks in one view.
    pub equivocations: u64,
}

struct BlockEntry {
    block: Arc<Block>,
    /// Local time the block was first inserted (pacing in replay mode).
    seen_ms: u64,
}

#[derive(Default)]
struct VoteAgg {
    voters: BTreeMap<Address, (BlockId, Height, VoteKind)>,
    groups: BTreeMap<(BlockId, Height, VoteKind), (u64, Vec<(Address, Sig65)>)>,
    formed: bool,
}

struct TimeoutAgg {
    msgs: BTreeMap<Address, (View, Sig65)>,
    stake: u64,
    best_qc: QuorumCert,
    formed: bool,
}

#[derive(Default, Clone)]
struct PeerInfo {
    committed_height: Height,
    high_qc_view: View,
    last_relay_ms: u64,
}

struct Inflight {
    peer: Address,
    deadline: u64,
}

type Pending = (Arc<Block>, Option<(Address, Option<TimeoutCert>)>);

/// Pipelined 2-chain HotStuff (Jolteon-style) core.
pub struct Core {
    cfg: CoreConfig,
    genesis_id: BlockId,
    is_validator: bool,
    now: u64,
    started: bool,

    safety: SafetyState,
    view: View,
    high_tc: Option<TimeoutCert>,
    proposed_in: View,
    my_timeout: Option<TimeoutMsg>,
    view_deadline: u64,
    timeout_streak: u32,
    next_rebroadcast: u64,
    next_propose_at: Option<u64>,
    next_status: u64,

    blocks: BTreeMap<BlockId, BlockEntry>,
    by_height: BTreeMap<Height, Vec<BlockId>>,
    qc_of: BTreeMap<BlockId, QuorumCert>,
    latest_block: Option<Arc<Block>>,
    verified_qcs: BTreeMap<(View, BlockId, Height), VoteKind>,
    verified_tcs: BTreeMap<View, BTreeSet<Hash32>>,
    proposals_per_view: BTreeMap<View, u32>,

    committed_height: Height,
    committed_id: BlockId,
    committed_view: View,
    last_commit_ms: u64,

    votes: BTreeMap<View, VoteAgg>,
    timeouts: BTreeMap<View, TimeoutAgg>,

    pending: BTreeMap<BlockId, Vec<Pending>>,
    pending_count: usize,
    fetching: BTreeMap<BlockId, (u64, Address)>,

    peers: BTreeMap<Address, PeerInfo>,
    inflight: Option<Inflight>,
    pending_snapshot: Option<(Address, Snapshot)>,
    snapshot_requested: Option<(Address, u64)>,

    exec_claims: BTreeMap<Height, Hash32>,
    override_cache: Option<(Height, Option<(TimestampMs, Hash32)>)>,

    /// Restart replay: next committed height to re-emit from the store.
    replay_next: Option<Height>,
    replay_top: Height,
    /// Commit/InstallSnapshot outputs held back while replaying.
    deferred: VecDeque<ConsensusOutput>,

    self_queue: VecDeque<ConsensusMsg>,
    out: Vec<ConsensusOutput>,
    last_timer: u64,
    stats: CoreStats,
}

const MAX_PENDING: usize = 256;
const RELAY_MIN_INTERVAL_MS: u64 = 300;
const FETCH_RETRY_MS: u64 = 1000;
const SYNC_TIMEOUT_MS: u64 = 3000;
const REPLAY_BATCH: usize = 32;
const MAX_FUTURE_TIMEOUT_VIEWS: u64 = 64;

impl Core {
    /// Build a core, resuming from the persisted safety state and committed
    /// log in `env.store()` if present.
    pub fn new(cfg: CoreConfig, env: &mut dyn ConsensusEnv) -> Core {
        let genesis = Arc::new(crypto::genesis_block(cfg.chain_id, cfg.genesis_time_ms));
        let genesis_id = genesis.id();
        let gqc = crypto::genesis_qc(cfg.chain_id, genesis_id);
        let store = env.store();
        let mut safety = store.load_safety().unwrap_or(SafetyState {
            last_voted_view: 0,
            last_timeout_view: 0,
            locked_qc: gqc.clone(),
            high_qc: gqc.clone(),
        });
        let committed_height = store.committed_height();
        let tip = if committed_height > 0 { store.committed_at(committed_height) } else { None };
        let (tip_block, committed_height) = match tip {
            Some(cb) => {
                if let Some(best) = cb.commit_proof.iter().max_by_key(|q| q.data.view) {
                    if best.data.view > safety.high_qc.data.view {
                        safety.high_qc = best.clone();
                        safety.locked_qc = best.clone();
                    }
                }
                (Arc::new(cb.block), committed_height)
            }
            None => (genesis.clone(), 0),
        };
        let committed_id = tip_block.id();
        let committed_view = tip_block.header.view;
        let view = (safety.high_qc.data.view + 1).max(safety.last_voted_view).max(safety.last_timeout_view).max(1);
        let is_validator = cfg.validators.contains(&cfg.me);
        let mut core = Core {
            genesis_id,
            is_validator,
            now: 0,
            started: false,
            proposed_in: safety.last_voted_view.max(safety.last_timeout_view),
            safety,
            view,
            high_tc: None,
            my_timeout: None,
            view_deadline: 0,
            timeout_streak: 0,
            next_rebroadcast: 0,
            next_propose_at: None,
            next_status: 0,
            blocks: BTreeMap::new(),
            by_height: BTreeMap::new(),
            qc_of: BTreeMap::new(),
            latest_block: None,
            verified_qcs: BTreeMap::new(),
            verified_tcs: BTreeMap::new(),
            proposals_per_view: BTreeMap::new(),
            committed_height,
            committed_id,
            committed_view,
            last_commit_ms: 0,
            votes: BTreeMap::new(),
            timeouts: BTreeMap::new(),
            pending: BTreeMap::new(),
            pending_count: 0,
            fetching: BTreeMap::new(),
            peers: BTreeMap::new(),
            inflight: None,
            pending_snapshot: None,
            snapshot_requested: None,
            exec_claims: BTreeMap::new(),
            override_cache: None,
            replay_next: None,
            replay_top: committed_height,
            deferred: VecDeque::new(),
            self_queue: VecDeque::new(),
            out: Vec::new(),
            last_timer: 0,
            stats: CoreStats::default(),
            cfg,
        };
        core.insert_raw(tip_block, 0);
        if committed_height > 0 {
            info!(committed_height, view = core.view, "consensus resuming from store");
        }
        core
    }

    pub fn stats(&self) -> &CoreStats {
        &self.stats
    }
    pub fn config(&self) -> &CoreConfig {
        &self.cfg
    }
    pub fn genesis_id(&self) -> BlockId {
        self.genesis_id
    }
    pub fn committed_id(&self) -> BlockId {
        self.committed_id
    }
    pub fn high_qc(&self) -> &QuorumCert {
        &self.safety.high_qc
    }
    pub fn safety_state(&self) -> &SafetyState {
        &self.safety
    }
    pub fn high_tc(&self) -> Option<&TimeoutCert> {
        self.high_tc.as_ref()
    }

    fn cert_ctx(&self) -> CertCtx<'_> {
        CertCtx { chain_id: self.cfg.chain_id, genesis_id: self.genesis_id, validators: &self.cfg.validators }
    }

    fn leader(&self, v: View) -> Address {
        self.cfg.validators.leader(v)
    }

    // ------------------------------------------------------------ output --

    fn send_to(&mut self, to: Address, msg: ConsensusMsg) {
        if to == self.cfg.me {
            self.self_queue.push_back(msg);
        } else {
            self.out.push(ConsensusOutput::Send { to, msg });
        }
    }

    fn broadcast(&mut self, msg: ConsensusMsg) {
        self.out.push(ConsensusOutput::Broadcast(msg));
    }

    fn emit_ordered(&mut self, o: ConsensusOutput) {
        if self.replay_next.is_some() || !self.deferred.is_empty() {
            self.deferred.push_back(o);
        } else {
            self.out.push(o);
        }
    }

    // ------------------------------------------------------------- entry --

    fn startup(&mut self, env: &mut dyn ConsensusEnv) {
        self.started = true;
        self.view_deadline = self.now + self.cfg.view_timeout(0);
        self.next_rebroadcast = self.now;
        self.next_status = self.now;
        self.last_commit_ms = self.now;
        let (eh, _) = env.exec_state();
        if eh < self.committed_height {
            self.replay_next = Some(eh + 1);
            self.replay_top = self.committed_height;
            info!(from = eh + 1, to = self.committed_height, "replaying committed blocks to executor");
        }
        // reload the uncommitted chain up to high_qc from the store
        let hq = self.safety.high_qc.data.block_id;
        if !self.blocks.contains_key(&hq) {
            if let Some(b) = env.store().get_block(&hq) {
                self.add_block(Arc::new(b), None, env);
            }
        }
    }

    fn replay_step(&mut self, env: &mut dyn ConsensusEnv) {
        let Some(mut h) = self.replay_next else { return };
        let mut n = 0;
        while h <= self.replay_top && n < REPLAY_BATCH {
            if let Some(cb) = env.store().committed_at(h) {
                self.out.push(ConsensusOutput::Commit(cb));
                h += 1;
            } else if let Some(s) = env.store().latest_snapshot().filter(|s| s.height >= h && s.height <= self.replay_top) {
                h = s.height + 1;
                self.out.push(ConsensusOutput::InstallSnapshot(s));
            } else {
                error!(height = h, "cannot replay committed height: pruned and no snapshot");
                h = self.replay_top + 1;
            }
            n += 1;
        }
        if h > self.replay_top {
            self.replay_next = None;
            self.out.extend(self.deferred.drain(..));
        } else {
            self.replay_next = Some(h);
        }
    }

    fn dispatch(&mut self, from: Address, msg: ConsensusMsg, env: &mut dyn ConsensusEnv) {
        match msg {
            ConsensusMsg::Proposal { block, tc } => self.on_proposal(from, block, tc, env),
            ConsensusMsg::Vote(v) => self.on_vote(from, v, env),
            ConsensusMsg::Timeout(t) => self.on_timeout(from, t, env),
            ConsensusMsg::SyncRequest { from: f, to } => self.on_sync_request(from, f, to, env),
            ConsensusMsg::SyncResponse { blocks, peer_committed_height } => {
                self.on_sync_response(from, blocks, peer_committed_height, env)
            }
            ConsensusMsg::FetchBlock { id } => self.on_fetch(from, id, env),
            ConsensusMsg::FetchBlockResponse { block } => {
                if let Some(b) = block {
                    self.on_fetch_response(from, b, env)
                }
            }
            ConsensusMsg::SnapshotRequest => {
                let s = env.snapshot();
                self.send_to(from, ConsensusMsg::SnapshotResponse(s));
            }
            ConsensusMsg::SnapshotResponse(s) => {
                if let Some(s) = s {
                    self.on_snapshot_response(from, s)
                }
            }
            ConsensusMsg::Status { committed_height, high_qc_view } => {
                self.on_status(from, committed_height, high_qc_view)
            }
        }
    }

    fn drain_self(&mut self, env: &mut dyn ConsensusEnv) {
        while let Some(m) = self.self_queue.pop_front() {
            let me = self.cfg.me;
            self.dispatch(me, m, env);
        }
    }

    // ------------------------------------------------------- certificates --

    fn check_qc(&mut self, qc: &QuorumCert, env: &dyn ConsensusEnv) -> Option<VoteKind> {
        let key = (qc.data.view, qc.data.block_id, qc.data.height);
        if qc.data.chain_id == self.cfg.chain_id {
            if let Some(k) = self.verified_qcs.get(&key) {
                return Some(*k);
            }
        }
        let verify = |d: &[u8; 32], s: &Sig65, a: &Address| env.verify(d, s, a);
        match self.cert_ctx().verify_qc(qc, &verify) {
            Ok(k) => {
                self.verified_qcs.insert(key, k);
                Some(k)
            }
            Err(e) => {
                debug!(err = %e, "invalid qc");
                None
            }
        }
    }

    fn check_tc(&mut self, tc: &TimeoutCert, env: &dyn ConsensusEnv) -> bool {
        let h = hash_borsh(tc);
        if self.verified_tcs.get(&tc.view).is_some_and(|s| s.contains(&h)) {
            return true;
        }
        if self.check_qc(&tc.high_qc, env).is_none() {
            return false;
        }
        let verify = |d: &[u8; 32], s: &Sig65, a: &Address| env.verify(d, s, a);
        match self.cert_ctx().verify_tc_sigs(tc, &verify) {
            Ok(()) => {
                self.verified_tcs.entry(tc.view).or_default().insert(h);
                true
            }
            Err(e) => {
                debug!(err = %e, "invalid tc");
                false
            }
        }
    }

    fn advance_view(&mut self, new_view: View, via_tc: bool) {
        if new_view <= self.view {
            return;
        }
        self.view = new_view;
        if via_tc {
            self.timeout_streak = self.timeout_streak.saturating_add(1);
        } else {
            self.timeout_streak = 0;
        }
        self.view_deadline = self.now + self.cfg.view_timeout(self.timeout_streak);
        self.my_timeout = None;
        self.next_propose_at = None;
        let keep = new_view.saturating_sub(1);
        self.votes = self.votes.split_off(&keep);
        self.timeouts = self.timeouts.split_off(&new_view);
        self.proposals_per_view = self.proposals_per_view.split_off(&keep);
        debug!(view = new_view, via_tc, "entered view");
    }

    /// Process a verified QC.
    fn process_qc(&mut self, qc: &QuorumCert, env: &mut dyn ConsensusEnv) {
        if qc.data.view > self.safety.high_qc.data.view {
            self.safety.high_qc = qc.clone();
            self.safety.locked_qc = qc.clone();
        }
        if qc.data.view > 0 && qc.data.height + self.cfg.memory_window >= self.committed_height {
            self.qc_of.entry(qc.data.block_id).or_insert_with(|| qc.clone());
        }
        self.advance_view(qc.data.view + 1, false);
        self.try_commit_from(qc, env);
    }

    /// Process a verified TC.
    fn process_tc(&mut self, tc: &TimeoutCert, env: &mut dyn ConsensusEnv) {
        if self.high_tc.as_ref().is_none_or(|t| tc.view > t.view) {
            self.high_tc = Some(tc.clone());
        }
        self.process_qc(&tc.high_qc, env);
        self.advance_view(tc.view + 1, true);
    }

    // ------------------------------------------------------------ blocks --

    fn insert_raw(&mut self, b: Arc<Block>, seen_ms: u64) {
        let id = b.id();
        self.by_height.entry(b.header.height).or_default().push(id);
        if b.header.height > 0
            && self.latest_block.as_ref().is_none_or(|l| b.header.justify.data.view > l.header.justify.data.view)
        {
            self.latest_block = Some(b.clone());
        }
        self.blocks.insert(id, BlockEntry { block: b, seen_ms });
    }

    fn structurally_valid(&self, b: &Block, parent: &Block) -> bool {
        let h = &b.header;
        let p = &parent.header;
        h.chain_id == self.cfg.chain_id
            && h.height == p.height + 1
            && h.justify.data.height == p.height
            && h.justify.data.view == p.view
            && h.view > h.justify.data.view
            && h.timestamp_ms > p.timestamp_ms
            && h.exec_height < h.height
    }

    /// Insert a block (payload hash already checked) and anything waiting on
    /// it. `fresh = Some((from, tc))` for a proposal received directly from
    /// its proposer: it may be voted on.
    fn add_block(&mut self, block: Arc<Block>, fresh: Option<(Address, Option<TimeoutCert>)>, env: &mut dyn ConsensusEnv) {
        let mut work: Vec<Pending> = vec![(block, fresh)];
        let mut from_store: BTreeSet<BlockId> = BTreeSet::new();
        while let Some((b, fresh)) = work.pop() {
            let id = b.id();
            if self.blocks.contains_key(&id) {
                if let Some((_, tc)) = fresh {
                    self.maybe_vote(&b, tc.as_ref(), env);
                }
                continue;
            }
            if self.check_qc(&b.header.justify, env).is_none() {
                continue;
            }
            let parent_id = b.header.parent;
            let ph = b.header.justify.data.height;
            if !self.blocks.contains_key(&parent_id) {
                if ph > self.committed_height && !from_store.contains(&parent_id) && from_store.len() < 4 * self.cfg.fetch_window as usize {
                    if let Some(pb) = env.store().get_block(&parent_id) {
                        from_store.insert(parent_id);
                        work.push((b, fresh));
                        work.push((Arc::new(pb), None));
                        continue;
                    }
                }
                if ph < self.committed_height || (ph == self.committed_height && parent_id != self.committed_id) {
                    continue; // stale fork
                }
                let peer = fresh.as_ref().map(|(f, _)| *f).unwrap_or(b.header.proposer);
                if ph > self.committed_height + self.cfg.fetch_window {
                    self.start_sync(peer);
                } else {
                    self.fetch(parent_id, peer);
                }
                if self.pending_count < MAX_PENDING {
                    self.pending_count += 1;
                    self.pending.entry(parent_id).or_default().push((b, fresh));
                }
                continue;
            }
            let parent = self.blocks[&parent_id].block.clone();
            if !self.structurally_valid(&b, &parent) {
                debug!(%id, "structurally invalid block");
                continue;
            }
            let pheight = parent.header.height;
            if pheight < self.committed_height || (pheight == self.committed_height && parent_id != self.committed_id) {
                continue;
            }
            if let Err(e) = env.store().put_block(&b) {
                error!(err = %e, "store.put_block failed");
            }
            self.insert_raw(b.clone(), self.now);
            self.fetching.remove(&id);
            let justify = b.header.justify.clone();
            self.process_qc(&justify, env);
            if let Some(qc) = self.qc_of.get(&id).cloned() {
                self.try_commit_from(&qc, env);
            }
            if let Some((_, tc)) = fresh {
                self.maybe_vote(&b, tc.as_ref(), env);
            }
            if let Some(children) = self.pending.remove(&id) {
                self.pending_count -= children.len();
                work.extend(children);
            }
        }
    }

    fn fetch(&mut self, id: BlockId, peer: Address) {
        if self.fetching.contains_key(&id) {
            return;
        }
        let peer = if peer == self.cfg.me { self.next_peer(peer) } else { peer };
        self.fetching.insert(id, (self.now, peer));
        self.send_to(peer, ConsensusMsg::FetchBlock { id });
    }

    fn next_peer(&self, after: Address) -> Address {
        let vs = &self.cfg.validators.validators;
        let n = vs.len();
        let i = vs.iter().position(|v| v.address == after).unwrap_or(0);
        for k in 1..=n {
            let a = vs[(i + k) % n].address;
            if a != self.cfg.me {
                return a;
            }
        }
        after
    }

    fn on_proposal(&mut self, from: Address, block: Block, tc: Option<TimeoutCert>, env: &mut dyn ConsensusEnv) {
        if block.header.chain_id != self.cfg.chain_id {
            return;
        }
        if let Some(t) = &tc {
            if !self.check_tc(t, env) {
                return;
            }
            self.process_tc(t, env);
        }
        let id = block.id();
        if id == self.genesis_id || block.header.height == 0 {
            return;
        }
        if self.check_qc(&block.header.justify, env).is_none() {
            return;
        }
        let justify = block.header.justify.clone();
        self.process_qc(&justify, env);
        let h = &block.header;
        let direct = from == h.proposer && h.proposer == self.leader(h.view);
        if self.blocks.contains_key(&id) {
            if direct {
                let b = self.blocks[&id].block.clone();
                self.maybe_vote(&b, tc.as_ref(), env);
            }
            return;
        }
        if !direct && !self.fetching.contains_key(&id) && !self.qc_of.contains_key(&id) {
            return; // relay: certificates only
        }
        if direct && !self.fetching.contains_key(&id) && !self.qc_of.contains_key(&id) {
            if h.view + 1 < self.view {
                return; // stale, uncertified
            }
            let c = self.proposals_per_view.entry(h.view).or_insert(0);
            if *c >= 2 {
                return; // equivocation flood
            }
            *c += 1;
        }
        if hash_borsh(&block.payload) != h.payload_hash {
            warn!(%from, "proposal payload hash mismatch");
            return;
        }
        let fresh = if direct { Some((from, tc)) } else { None };
        self.add_block(Arc::new(block), fresh, env);
    }

    fn on_fetch(&mut self, from: Address, id: BlockId, env: &mut dyn ConsensusEnv) {
        let b = match self.blocks.get(&id) {
            Some(e) => Some((*e.block).clone()),
            None => env.store().get_block(&id),
        };
        self.send_to(from, ConsensusMsg::FetchBlockResponse { block: b });
    }

    fn on_fetch_response(&mut self, _from: Address, b: Block, env: &mut dyn ConsensusEnv) {
        let id = b.id();
        if !self.fetching.contains_key(&id) || hash_borsh(&b.payload) != b.header.payload_hash {
            return;
        }
        self.add_block(Arc::new(b), None, env);
    }

    // ------------------------------------------------------------ voting --

    fn override_for(&mut self, height: Height, env: &mut dyn ConsensusEnv) -> Option<(TimestampMs, Hash32)> {
        if let Some((h, v)) = &self.override_cache {
            if *h == height {
                return *v;
            }
        }
        let v = env.proposal_override(height).map(|(ts, p)| (ts, hash_borsh(&p)));
        self.override_cache = Some((height, v));
        v
    }

    fn maybe_vote(&mut self, b: &Arc<Block>, tc: Option<&TimeoutCert>, env: &mut dyn ConsensusEnv) {
        if !self.is_validator {
            return;
        }
        let h = &b.header;
        if h.view != self.view || h.view <= self.safety.last_voted_view || h.view <= self.safety.last_timeout_view {
            return;
        }
        if h.proposer != self.leader(h.view) {
            return;
        }
        let kind = VoteKind::of_header(h);
        if kind == VoteKind::AfterTimeout {
            let t = tc.filter(|t| t.view + 1 == h.view).or(self.high_tc.as_ref().filter(|t| t.view + 1 == h.view));
            match t {
                None => return,
                Some(t) if h.justify.data.view < t.high_qc.data.view => {
                    warn!(view = h.view, "proposal does not extend the TC's highest QC");
                    return;
                }
                _ => {}
            }
        }
        match self.override_for(h.height, env) {
            Some((ts, ph)) => {
                if h.timestamp_ms != ts || h.payload_hash != ph {
                    warn!(height = h.height, "proposal differs from replay override; not voting");
                    return;
                }
            }
            None => {
                if h.timestamp_ms > self.now + self.cfg.max_future_drift_ms {
                    debug!(ts = h.timestamp_ms, now = self.now, "proposal timestamp too far in the future");
                    return;
                }
            }
        }
        if let Some(r) = env.root_at(h.exec_height) {
            if r != h.exec_root {
                self.stats.divergences += 1;
                error!(
                    height = h.exec_height,
                    local_root = %r,
                    claimed_root = %h.exec_root,
                    proposer = %h.proposer,
                    "STATE DIVERGENCE: proposal exec root differs from local root; refusing to vote"
                );
                return;
            }
        }
        self.safety.last_voted_view = h.view;
        if let Err(e) = env.store().save_safety(&self.safety) {
            error!(err = %e, "failed to persist safety state; not voting");
            return;
        }
        let data = VoteData { chain_id: self.cfg.chain_id, block_id: b.id(), view: h.view, height: h.height };
        let signature = env.sign(&crypto::vote_digest(&data, kind));
        let vote = Vote { data, voter: self.cfg.me, signature };
        self.stats.votes += 1;
        let next = self.leader(h.view + 1);
        self.send_to(next, ConsensusMsg::Vote(vote));
    }

    fn on_vote(&mut self, from: Address, vote: Vote, env: &mut dyn ConsensusEnv) {
        if vote.voter != from || vote.data.chain_id != self.cfg.chain_id {
            return;
        }
        let stake = self.cfg.validators.stake_of(&from);
        if stake == 0 {
            return;
        }
        let v = vote.data.view;
        if v == 0 || self.leader(v + 1) != self.cfg.me || v + 1 < self.view {
            return;
        }
        if let Some(agg) = self.votes.get(&v) {
            if let Some(prev) = agg.voters.get(&from) {
                if prev.0 != vote.data.block_id {
                    self.stats.equivocations += 1;
                    warn!(voter = %from, view = v, "equivocating vote");
                }
                return;
            }
            if agg.formed {
                return;
            }
        }
        let kind = [VoteKind::Direct, VoteKind::AfterTimeout]
            .into_iter()
            .find(|k| env.verify(&crypto::vote_digest(&vote.data, *k), &vote.signature, &from));
        let Some(kind) = kind else { return };
        let quorum = self.cfg.validators.quorum_stake();
        let agg = self.votes.entry(v).or_default();
        let key = (vote.data.block_id, vote.data.height, kind);
        agg.voters.insert(from, key);
        let g = agg.groups.entry(key).or_insert((0, vec![]));
        g.0 += stake;
        g.1.push((from, vote.signature));
        if g.0 >= quorum {
            agg.formed = true;
            let mut sigs = g.1.clone();
            sigs.sort_by_key(|(a, _)| *a);
            let qc = QuorumCert { data: vote.data.clone(), signatures: sigs };
            self.verified_qcs.insert((v, vote.data.block_id, vote.data.height), kind);
            self.stats.qcs_formed += 1;
            debug!(view = v, height = vote.data.height, "formed QC");
            self.qc_of.insert(qc.data.block_id, qc.clone());
            self.process_qc(&qc, env);
            if !self.blocks.contains_key(&qc.data.block_id) {
                let peer = if from == self.cfg.me { self.leader(v) } else { from };
                self.fetch(qc.data.block_id, peer);
            }
        }
    }

    // ---------------------------------------------------------- timeouts --

    fn local_timeout(&mut self, env: &mut dyn ConsensusEnv) {
        let v = self.view;
        if self.safety.last_timeout_view < v {
            self.safety.last_timeout_view = v;
            self.stats.timeouts += 1;
            info!(view = v, high_qc = self.safety.high_qc.data.view, "view timeout");
        }
        if let Err(e) = env.store().save_safety(&self.safety) {
            error!(err = %e, "failed to persist safety state; not sending timeout");
            return;
        }
        let data = TimeoutData { chain_id: self.cfg.chain_id, view: v, high_qc_view: self.safety.high_qc.data.view };
        let signature = env.sign(&crypto::timeout_digest(&data));
        let msg = TimeoutMsg { data, high_qc: self.safety.high_qc.clone(), sender: self.cfg.me, signature };
        self.my_timeout = Some(msg.clone());
        self.next_rebroadcast = self.now + self.cfg.timeout_rebroadcast_ms;
        self.broadcast(ConsensusMsg::Timeout(msg.clone()));
        let me = self.cfg.me;
        self.on_timeout(me, msg, env);
    }

    fn on_timeout(&mut self, from: Address, t: TimeoutMsg, env: &mut dyn ConsensusEnv) {
        if t.sender != from || t.data.chain_id != self.cfg.chain_id {
            return;
        }
        let stake = self.cfg.validators.stake_of(&from);
        if stake == 0 || t.data.high_qc_view != t.high_qc.data.view || t.data.high_qc_view >= t.data.view {
            return;
        }
        let v = t.data.view;
        if v > self.view + MAX_FUTURE_TIMEOUT_VIEWS {
            return;
        }
        if self.timeouts.get(&v).is_some_and(|a| a.formed || a.msgs.contains_key(&from)) {
            return;
        }
        if from != self.cfg.me && !env.verify(&crypto::timeout_digest(&t.data), &t.signature, &from) {
            return;
        }
        if self.check_qc(&t.high_qc, env).is_none() {
            return;
        }
        self.process_qc(&t.high_qc, env);
        if v < self.view {
            self.relay(from);
            return;
        }
        let quorum = self.cfg.validators.quorum_stake();
        let validity = self.cfg.validators.validity_stake();
        let agg = self.timeouts.entry(v).or_insert_with(|| TimeoutAgg {
            msgs: BTreeMap::new(),
            stake: 0,
            best_qc: t.high_qc.clone(),
            formed: false,
        });
        agg.msgs.insert(from, (t.data.high_qc_view, t.signature));
        agg.stake += stake;
        if t.high_qc.data.view > agg.best_qc.data.view {
            agg.best_qc = t.high_qc.clone();
        }
        if agg.stake >= quorum {
            agg.formed = true;
            let tc = TimeoutCert {
                view: v,
                timeouts: agg.msgs.iter().map(|(a, (hv, s))| (*a, *hv, *s)).collect(),
                high_qc: agg.best_qc.clone(),
            };
            self.verified_tcs.entry(v).or_default().insert(hash_borsh(&tc));
            self.stats.tcs_formed += 1;
            debug!(view = v, "formed TC");
            self.process_tc(&tc, env);
        } else if agg.stake >= validity && v == self.view && self.safety.last_timeout_view < v && self.is_validator {
            // f+1 honest-stake evidence that the view is failing: join.
            self.local_timeout(env);
        }
    }

    // ------------------------------------------------------------ commit --

    fn try_commit_from(&mut self, qc: &QuorumCert, env: &mut dyn ConsensusEnv) {
        let Some(b1) = self.blocks.get(&qc.data.block_id).map(|e| e.block.clone()) else { return };
        if b1.header.height <= self.committed_height + 1 {
            return;
        }
        let Some(b0) = self.blocks.get(&b1.header.parent).map(|e| e.block.clone()) else { return };
        if b1.header.view == b0.header.view + 1 {
            self.commit_chain(b0, vec![b1.header.justify.clone(), qc.clone()], env);
        }
    }

    fn commit_chain(&mut self, b0: Arc<Block>, trigger: Vec<QuorumCert>, env: &mut dyn ConsensusEnv) {
        let mut chain = vec![b0];
        loop {
            let cur = chain.last().unwrap();
            if cur.header.height == self.committed_height + 1 {
                break;
            }
            match self.blocks.get(&cur.header.parent) {
                Some(e) => {
                    let b = e.block.clone();
                    chain.push(b);
                }
                None => {
                    warn!(missing = %cur.header.parent, "commit: ancestor missing");
                    return;
                }
            }
        }
        let bottom = chain.last().unwrap();
        if bottom.header.parent != self.committed_id {
            self.stats.safety_violations += 1;
            error!(
                height = bottom.header.height,
                committed = %self.committed_id,
                "SAFETY VIOLATION: certified chain conflicts with committed chain"
            );
            return;
        }
        chain.reverse();
        let n = chain.len();
        for i in 0..n {
            let proof = if i + 1 == n {
                trigger.clone()
            } else {
                vec![chain[i + 1].header.justify.clone(), trigger[0].clone(), trigger[1].clone()]
            };
            let cb = CommittedBlock { block: (*chain[i]).clone(), commit_proof: proof };
            self.commit_one(cb, env);
        }
    }

    fn commit_one(&mut self, cb: CommittedBlock, env: &mut dyn ConsensusEnv) {
        let h = &cb.block.header;
        debug_assert_eq!(h.height, self.committed_height + 1);
        if let Err(e) = env.store().put_committed(&cb) {
            error!(err = %e, "store.put_committed failed");
        }
        self.committed_height = h.height;
        self.committed_id = h.id();
        self.committed_view = h.view;
        self.last_commit_ms = self.now;
        self.stats.commits += 1;
        let (eh, er) = (h.exec_height, h.exec_root);
        self.record_exec_claim(eh, er, env);
        debug!(height = self.committed_height, "committed");
        self.emit_ordered(ConsensusOutput::Commit(cb));
        self.prune();
    }

    fn record_exec_claim(&mut self, eh: Height, root: Hash32, env: &mut dyn ConsensusEnv) {
        if let Some(prev) = self.exec_claims.get(&eh) {
            if *prev != root {
                self.stats.divergences += 1;
                error!(height = eh, a = %prev, b = %root, "STATE DIVERGENCE: committed headers certify different roots");
            }
        } else {
            self.exec_claims.insert(eh, root);
        }
        if let Some(local) = env.root_at(eh) {
            if local != root {
                self.stats.divergences += 1;
                error!(height = eh, local_root = %local, certified_root = %root, "STATE DIVERGENCE: local root differs from certified root");
            }
        }
    }

    fn on_executed(&mut self, height: Height, root: Hash32) {
        if let Some(c) = self.exec_claims.get(&height) {
            if *c != root {
                self.stats.divergences += 1;
                error!(height, local_root = %root, certified_root = %c, "STATE DIVERGENCE: executed root differs from certified root");
            }
        }
    }

    fn prune(&mut self) {
        let keep_from = self.committed_height.saturating_sub(self.cfg.memory_window);
        if self.by_height.keys().next().is_some_and(|h| *h < keep_from) {
            let keep = self.by_height.split_off(&keep_from);
            let old = std::mem::replace(&mut self.by_height, keep);
            for id in old.into_values().flatten() {
                self.blocks.remove(&id);
            }
            self.qc_of.retain(|_, q| q.data.height >= keep_from);
            let ch = self.committed_height;
            self.pending.retain(|_, v| {
                v.retain(|(b, _)| b.header.height > ch);
                !v.is_empty()
            });
            self.pending_count = self.pending.values().map(|v| v.len()).sum();
        }
        let vk = self.committed_view.saturating_sub(4 * self.cfg.memory_window);
        if self.verified_qcs.keys().next().is_some_and(|k| k.0 < vk) {
            self.verified_qcs = self.verified_qcs.split_off(&(vk, Hash32::ZERO, 0));
        }
        if self.verified_tcs.keys().next().is_some_and(|k| *k < vk) {
            self.verified_tcs = self.verified_tcs.split_off(&vk);
        }
        let ek = self.committed_height.saturating_sub(10_000);
        if self.exec_claims.keys().next().is_some_and(|k| *k < ek) {
            self.exec_claims = self.exec_claims.split_off(&ek);
        }
    }

    // --------------------------------------------------------- proposing --

    fn try_propose(&mut self, env: &mut dyn ConsensusEnv) {
        let v = self.view;
        if !self.is_validator || self.leader(v) != self.cfg.me || self.proposed_in >= v || self.safety.last_timeout_view >= v {
            return;
        }
        let hq = self.safety.high_qc.clone();
        let via_tc = hq.data.view + 1 != v;
        if via_tc && self.high_tc.as_ref().is_none_or(|t| t.view + 1 != v) {
            return;
        }
        let Some(pe) = self.blocks.get(&hq.data.block_id) else {
            let peer = self.next_peer(self.cfg.me);
            self.fetch(hq.data.block_id, peer);
            return;
        };
        let parent = pe.block.clone();
        let parent_seen = pe.seen_ms;
        let height = parent.header.height + 1;
        let replay = self.override_for(height, env).is_some();
        let earliest = if replay {
            parent_seen + self.cfg.min_block_interval_ms
        } else {
            parent.header.timestamp_ms + self.cfg.min_block_interval_ms
        };
        if self.now < earliest {
            self.next_propose_at = Some(earliest);
            return;
        }
        self.next_propose_at = None;
        let ov = if replay { env.proposal_override(height) } else { None };
        let (timestamp_ms, payload) = match ov {
            Some((ts, p)) => (ts, p),
            None => {
                let p = env.payload(self.cfg.max_block_txs, self.cfg.max_block_bytes);
                (self.now.max(parent.header.timestamp_ms + 1), p)
            }
        };
        if timestamp_ms <= parent.header.timestamp_ms {
            error!(height, "replay override timestamp not after parent; cannot propose");
            self.proposed_in = v;
            return;
        }
        let (mut eh, mut er) = env.exec_state();
        if eh >= height {
            eh = parent.header.exec_height;
            er = parent.header.exec_root;
        }
        let payload_hash = hash_borsh(&payload);
        let header = BlockHeader {
            chain_id: self.cfg.chain_id,
            height,
            view: v,
            parent: parent.id(),
            proposer: self.cfg.me,
            timestamp_ms,
            payload_hash,
            exec_height: eh,
            exec_root: er,
            justify: hq,
        };
        let block = Block { header, payload };
        let tc = if via_tc { self.high_tc.clone() } else { None };
        self.proposed_in = v;
        self.stats.proposals += 1;
        debug!(view = v, height, txs = block.payload.len(), "proposing");
        self.broadcast(ConsensusMsg::Proposal { block: block.clone(), tc: tc.clone() });
        let me = self.cfg.me;
        self.add_block(Arc::new(block), Some((me, tc)), env);
    }

    // --------------------------------------------------------- view sync --

    fn relay(&mut self, peer: Address) {
        if peer == self.cfg.me {
            return;
        }
        let now = self.now;
        let p = self.peers.entry(peer).or_default();
        if p.last_relay_ms != 0 && now < p.last_relay_ms + RELAY_MIN_INTERVAL_MS {
            return;
        }
        p.last_relay_ms = now.max(1);
        let carrier = self.latest_block.clone().or_else(|| {
            self.blocks.get(&self.committed_id).map(|e| e.block.clone()).filter(|b| b.header.height > 0)
        });
        let Some(b) = carrier else { return };
        let tc = self.high_tc.clone();
        self.send_to(peer, ConsensusMsg::Proposal { block: (*b).clone(), tc });
    }

    fn on_status(&mut self, from: Address, ch: Height, hqv: View) {
        let p = self.peers.entry(from).or_default();
        p.committed_height = p.committed_height.max(ch);
        p.high_qc_view = hqv;
        if hqv < self.safety.high_qc.data.view {
            self.relay(from);
        }
        self.maybe_sync(from);
    }

    // -------------------------------------------------------- state sync --

    fn maybe_sync(&mut self, peer: Address) {
        let pch = self.peers.get(&peer).map(|p| p.committed_height).unwrap_or(0);
        if pch <= self.committed_height {
            return;
        }
        let stalled = self.now >= self.last_commit_ms + self.cfg.sync_stall_ms;
        if pch > self.committed_height + self.cfg.sync_gap || stalled {
            self.start_sync(peer);
        }
    }

    fn start_sync(&mut self, peer: Address) {
        if self.inflight.is_some() || self.replay_next.is_some() || peer == self.cfg.me {
            return;
        }
        let from = match &self.pending_snapshot {
            Some((_, s)) => s.height + 1,
            None => self.committed_height + 1,
        };
        self.inflight = Some(Inflight { peer, deadline: self.now + SYNC_TIMEOUT_MS });
        self.send_to(peer, ConsensusMsg::SyncRequest { from, to: from + self.cfg.sync_batch - 1 });
    }

    fn on_sync_request(&mut self, from: Address, f: Height, t: Height, env: &mut dyn ConsensusEnv) {
        let f = f.max(1);
        let top = self.committed_height.min(t).min(f.saturating_add(self.cfg.sync_batch.saturating_sub(1)));
        let mut blocks: Vec<CommittedBlock> = vec![];
        let mut txs = 0usize;
        let mut h = f;
        while h <= top {
            let Some(cb) = env.store().committed_at(h) else { break };
            txs += cb.block.payload.len();
            blocks.push(cb);
            h += 1;
            if txs >= self.cfg.sync_max_txs {
                break;
            }
        }
        // make sure the run ends with a self-proving (direct) commit proof
        let mut extra = 0;
        while blocks.last().is_some_and(|l| l.commit_proof.len() != 2) && h <= self.committed_height && extra < 64 {
            let Some(cb) = env.store().committed_at(h) else { break };
            blocks.push(cb);
            h += 1;
            extra += 1;
        }
        let pch = self.committed_height;
        self.send_to(from, ConsensusMsg::SyncResponse { blocks, peer_committed_height: pch });
    }

    fn on_sync_response(&mut self, from: Address, blocks: Vec<CommittedBlock>, pch: Height, env: &mut dyn ConsensusEnv) {
        {
            let p = self.peers.entry(from).or_default();
            p.committed_height = p.committed_height.max(pch);
        }
        if self.inflight.as_ref().is_some_and(|i| i.peer == from) {
            self.inflight = None;
        }
        let verify = |d: &[u8; 32], s: &Sig65, a: &Address| env.verify(d, s, a);

        if let Some((_, snap)) = self.pending_snapshot.clone() {
            if blocks.first().is_some_and(|b| b.block.header.height == snap.height + 1) {
                self.pending_snapshot = None;
                if snap.height <= self.committed_height {
                    return;
                }
                let n = self.cert_ctx().verify_committed_run(&blocks, None, &verify);
                let certified = blocks[..n]
                    .iter()
                    .any(|cb| cb.block.header.exec_height == snap.height && cb.block.header.exec_root == snap.state_root);
                if !certified {
                    warn!(height = snap.height, "snapshot root not certified by any verified committed header; rejecting");
                    return;
                }
                info!(height = snap.height, root = %snap.state_root, "installing verified snapshot");
                if let Err(e) = env.store().save_snapshot(&snap) {
                    error!(err = %e, "save_snapshot failed");
                }
                self.committed_height = snap.height;
                self.committed_id = blocks[0].block.header.parent;
                self.stats.snapshots_installed += 1;
                self.emit_ordered(ConsensusOutput::InstallSnapshot(snap));
                self.apply_synced(blocks, n, env);
                return;
            }
        }

        let start = blocks.iter().position(|b| b.block.header.height == self.committed_height + 1);
        let Some(start) = start else {
            let none_useful = blocks.iter().all(|b| b.block.header.height <= self.committed_height);
            if none_useful && pch > self.committed_height && blocks.is_empty() {
                // peer pruned the range we need: fall back to a snapshot
                let recent = self.snapshot_requested.as_ref().is_some_and(|(_, t)| self.now < t + 5000);
                if !recent {
                    info!(peer = %from, "peer cannot serve blocks; requesting snapshot");
                    self.snapshot_requested = Some((from, self.now));
                    self.send_to(from, ConsensusMsg::SnapshotRequest);
                }
            }
            return;
        };
        let mut blocks = blocks;
        blocks.drain(..start);
        let anchor = Some((self.committed_height, self.committed_id));
        let n = self.cert_ctx().verify_committed_run(&blocks, anchor, &verify);
        if n == 0 {
            warn!(peer = %from, "sync response failed verification");
            return;
        }
        self.apply_synced(blocks, n, env);
        if pch > self.committed_height {
            self.start_sync(from);
        }
    }

    fn apply_synced(&mut self, mut blocks: Vec<CommittedBlock>, n: usize, env: &mut dyn ConsensusEnv) {
        blocks.truncate(n);
        let last_proof = blocks.last().map(|b| b.commit_proof.clone()).unwrap_or_default();
        let last_block = blocks.last().map(|b| Arc::new(b.block.clone()));
        for cb in blocks {
            if cb.block.header.height != self.committed_height + 1 {
                break;
            }
            self.stats.synced_blocks += 1;
            self.commit_one(cb, env);
        }
        if let Some(b) = last_block {
            if b.id() == self.committed_id && !self.blocks.contains_key(&self.committed_id) {
                self.insert_raw(b, self.now);
            }
        }
        for qc in last_proof {
            self.process_qc(&qc, env);
        }
        // retry waiting proposals now that the chain advanced
        let waiting: Vec<BlockId> = self.pending.keys().copied().filter(|k| self.blocks.contains_key(k)).collect();
        for k in waiting {
            if let Some(children) = self.pending.remove(&k) {
                self.pending_count -= children.len();
                for (b, fresh) in children {
                    self.add_block(b, fresh, env);
                }
            }
        }
    }

    fn on_snapshot_response(&mut self, from: Address, s: Snapshot) {
        if !self.snapshot_requested.as_ref().is_some_and(|(p, _)| *p == from) || s.height <= self.committed_height {
            return;
        }
        info!(peer = %from, height = s.height, "received snapshot; fetching certifying headers");
        self.snapshot_requested = None;
        self.pending_snapshot = Some((from, s));
        self.inflight = None;
        self.start_sync(from);
    }

    // -------------------------------------------------------------- tick --

    fn on_tick(&mut self, env: &mut dyn ConsensusEnv) {
        if self.is_validator {
            if self.safety.last_timeout_view < self.view {
                if self.now >= self.view_deadline {
                    self.local_timeout(env);
                }
            } else if self.now >= self.next_rebroadcast {
                match self.my_timeout.clone() {
                    Some(t) if t.data.view == self.view && t.data.high_qc_view == self.safety.high_qc.data.view => {
                        self.next_rebroadcast = self.now + self.cfg.timeout_rebroadcast_ms;
                        self.broadcast(ConsensusMsg::Timeout(t));
                    }
                    _ => self.local_timeout(env),
                }
            }
            self.try_propose(env);
        }
        if self.now >= self.next_status {
            self.next_status = self.now + self.cfg.status_interval_ms;
            let st = ConsensusMsg::Status { committed_height: self.committed_height, high_qc_view: self.safety.high_qc.data.view };
            self.broadcast(st);
            // stalled? sync from the most advanced peer
            if self.now >= self.last_commit_ms + self.cfg.sync_stall_ms {
                if let Some((p, _)) = self
                    .peers
                    .iter()
                    .filter(|(_, i)| i.committed_height > self.committed_height)
                    .max_by_key(|(a, i)| (i.committed_height, std::cmp::Reverse(**a)))
                {
                    let p = *p;
                    self.start_sync(p);
                }
            }
        }
        if self.inflight.as_ref().is_some_and(|i| self.now >= i.deadline) {
            self.inflight = None;
            if self.pending_snapshot.is_some() {
                self.pending_snapshot = None;
            }
        }
        let now = self.now;
        let retry: Vec<(BlockId, Address)> =
            self.fetching.iter().filter(|(_, (t, _))| now >= t + FETCH_RETRY_MS).map(|(id, (_, p))| (*id, *p)).collect();
        for (id, p) in retry {
            let np = self.next_peer(p);
            self.fetching.insert(id, (now, np));
            self.send_to(np, ConsensusMsg::FetchBlock { id });
        }
    }

    fn emit_timer(&mut self) {
        let mut t = self.next_status;
        if self.is_validator {
            if self.safety.last_timeout_view < self.view {
                t = t.min(self.view_deadline);
            } else {
                t = t.min(self.next_rebroadcast);
            }
        }
        if let Some(p) = self.next_propose_at {
            t = t.min(p);
        }
        if let Some(i) = &self.inflight {
            t = t.min(i.deadline);
        }
        if let Some((ts, _)) = self.fetching.values().min_by_key(|(ts, _)| *ts) {
            t = t.min(ts + FETCH_RETRY_MS);
        }
        if self.replay_next.is_some() {
            t = self.now;
        }
        if t != self.last_timer || t <= self.now {
            self.last_timer = t;
            self.out.push(ConsensusOutput::SetTimer { at_ms: t });
        }
    }
}

impl ConsensusCore for Core {
    fn handle(&mut self, input: ConsensusInput, env: &mut dyn ConsensusEnv) -> Vec<ConsensusOutput> {
        self.now = env.now_ms();
        if !self.started {
            self.startup(env);
        }
        match input {
            ConsensusInput::Message { from, msg } => {
                if from != self.cfg.me {
                    self.dispatch(from, msg, env);
                }
            }
            ConsensusInput::Tick => {}
            ConsensusInput::Executed { height, root } => self.on_executed(height, root),
        }
        self.drain_self(env);
        self.on_tick(env);
        self.drain_self(env);
        self.replay_step(env);
        self.emit_timer();
        std::mem::take(&mut self.out)
    }

    fn committed_height(&self) -> Height {
        self.committed_height
    }

    fn current_view(&self) -> View {
        self.view
    }
}
