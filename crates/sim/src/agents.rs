//! Deterministic agent population (ChaCha20 seeded from the run seed).
//!
//! Every decision is driven by seeded ChaCha20 streams (one per account, one
//! for the scheduler, one per validator) and by the [`World`] observation;
//! no wall clock, no hash-map iteration. The population emits
//! [`Unsigned`] actions which are signed afterwards (in parallel, order
//! preserved).

use crate::app::World;
use crate::genesis::{genesis_balance_usd, purr_token_str, SimKeys};
use crate::px::*;
use crate::sign::{Corruption, Signer, Unsigned};
use interfaces::*;
use oracle::feeds::{Skew, SynthParams, SyntheticFeed};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AgentKind {
    MarketMaker,
    Taker,
    TrendFollower,
    TpSl,
    Leverage,
    Transfer,
    WashTrader,
    LiqHunter,
    Degen,
}

pub const ALL_KINDS: [AgentKind; 9] = [
    AgentKind::MarketMaker,
    AgentKind::Taker,
    AgentKind::TrendFollower,
    AgentKind::TpSl,
    AgentKind::Leverage,
    AgentKind::Transfer,
    AgentKind::WashTrader,
    AgentKind::LiqHunter,
    AgentKind::Degen,
];

fn kind_ix(k: AgentKind) -> usize {
    ALL_KINDS.iter().position(|x| *x == k).unwrap()
}

/// Per-mille shares of accounts by kind (remainder -> takers).
const SHARES: [(AgentKind, u32); 9] = [
    (AgentKind::MarketMaker, 30),
    (AgentKind::Taker, 280),
    (AgentKind::TrendFollower, 100),
    (AgentKind::TpSl, 100),
    (AgentKind::Leverage, 80),
    (AgentKind::Transfer, 80),
    (AgentKind::WashTrader, 80),
    (AgentKind::LiqHunter, 30),
    (AgentKind::Degen, 220),
];

/// Deterministic kind assignment (contiguous blocks in `ALL_KINDS` order).
/// Small populations get at least one account of each kind while possible
/// (market makers first, then takers, degens, ...).
pub fn assign_kinds(n: usize) -> Vec<AgentKind> {
    let mut counts = [0usize; 9];
    for (k, s) in SHARES {
        counts[kind_ix(k)] = n * s as usize / 1000;
    }
    let priority = [
        AgentKind::MarketMaker,
        AgentKind::Taker,
        AgentKind::Degen,
        AgentKind::WashTrader,
        AgentKind::WashTrader,
        AgentKind::TrendFollower,
        AgentKind::TpSl,
        AgentKind::Leverage,
        AgentKind::Transfer,
        AgentKind::LiqHunter,
    ];
    let mut total: usize = counts.iter().sum();
    let mut want = [0usize; 9];
    for k in priority {
        want[kind_ix(k)] += 1;
    }
    for k in priority {
        let ix = kind_ix(k);
        if counts[ix] < want[ix] && total < n {
            counts[ix] += 1;
            total += 1;
        }
    }
    while total > n {
        // only possible when n is tiny: trim the largest
        let ix = (0..9).max_by_key(|i| (counts[*i], *i)).unwrap();
        counts[ix] -= 1;
        total -= 1;
    }
    counts[kind_ix(AgentKind::Taker)] += n - total;
    let mut out = Vec::with_capacity(n);
    for k in ALL_KINDS {
        out.extend(std::iter::repeat(k).take(counts[kind_ix(k)]));
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    /// Full agent mix + validators (gen).
    Mixed,
    /// AC3: market makers with 50-entry batches + IOC takers, no validators.
    Bench,
}

#[derive(Clone, Debug)]
pub struct PopParams {
    pub seed: u64,
    pub mode: Mode,
    pub invalid_ppm: u32,
    pub skewed_validators: usize,
    /// Emit validator txs (oracle, bridge). Off for live load.
    pub validators: bool,
}

#[derive(Clone, Debug)]
struct Quote {
    cloid: Cloid,
    is_buy: bool,
    oid: Option<Oid>,
}

#[derive(Clone, Debug)]
struct Acct {
    addr: Address,
    agent_addr: Address,
    kind: AgentKind,
    rng: ChaCha20Rng,
    next_nonce: u64,
    agent_nonce: u64,
    cloid_ctr: u64,
    quotes: Vec<Vec<Quote>>,
    /// Market-maker ladder params.
    levels: usize,
    half_spread_bps: i64,
    step_bps: i64,
    quote_usd: i128,
    home: AssetId,
    partner: Option<usize>,
    lev_set: bool,
    agent_approved: bool,
    acted_at: u64,
    bench_phase: Vec<u8>,
    /// Ids of passive (trend / hunter) orders known from the last refresh.
    resting: Vec<(AssetId, Oid, bool)>,
}

struct ValAgent {
    feed: SyntheticFeed,
    nonce: u64,
    skewed: bool,
    rng: ChaCha20Rng,
}

#[derive(Default)]
struct BridgeSim {
    next_deposit: u64,
    /// (due height, validator, attestation)
    scheduled: Vec<(u64, usize, DepositAttestation)>,
    signed: BTreeSet<(u64, usize)>,
    finalized: BTreeSet<(u64, usize)>,
}

/// Counters for reporting.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PopStats {
    pub turns_by_kind: Vec<(String, u64)>,
    pub actions_by_kind: Vec<(String, u64)>,
    pub invalid: u64,
    pub oracle_txs: u64,
    pub deposit_attestations: u64,
    pub withdrawal_sigs: u64,
    pub withdrawal_finalized: u64,
    pub liq_pushes: u64,
}

pub struct Population {
    pub params: PopParams,
    markets: Vec<MarketSpec>,
    n_validators: usize,
    quorum_count: usize,
    bridge_params: BridgeParams,
    accts: Vec<Acct>,
    by_kind: Vec<Vec<usize>>,
    vals: Vec<ValAgent>,
    ref_feed: SyntheticFeed,
    fair: Vec<Px>,
    ema_fast: Vec<i128>,
    ema_slow: Vec<i128>,
    rng: ChaCha20Rng,
    bridge: BridgeSim,
    mm_acted: Vec<u64>,
    acted_now: Vec<usize>,
    refresh_cursor: usize,
    watch: BTreeSet<usize>,
    height: u64,
    turns: [u64; 9],
    actions: [u64; 9],
    stats: PopStats,
}

fn sub_rng(seed: u64, domain: &str, i: u64) -> ChaCha20Rng {
    let h = blake3::hash(&[&seed.to_le_bytes()[..], domain.as_bytes(), &i.to_le_bytes()].concat());
    ChaCha20Rng::from_seed(*h.as_bytes())
}

/// Common reference-price seed derived from the run seed (validators'
/// synthetic feeds and the agents' "fair" price share it).
pub fn feed_common_seed(seed: u64) -> u64 {
    u64::from_le_bytes(blake3::hash(&[&seed.to_le_bytes()[..], b"feed"].concat()).as_bytes()[..8].try_into().unwrap())
}

impl Population {
    pub fn new(g: &Genesis, keys: &SimKeys, params: PopParams) -> Self {
        let markets = g.markets.clone();
        let nm = markets.len();
        let kinds = &keys.kinds;
        let mut by_kind = vec![Vec::new(); 9];
        for (i, k) in kinds.iter().enumerate() {
            by_kind[kind_ix(*k)].push(i);
        }
        let wash = by_kind[kind_ix(AgentKind::WashTrader)].clone();
        let mut accts: Vec<Acct> = keys
            .accounts
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let mut rng = sub_rng(params.seed, "acct", i as u64);
                let home = (rng.gen_range(0..100u32) % nm as u32) as AssetId;
                Acct {
                    addr: w.address(),
                    agent_addr: keys.agents[i].address(),
                    kind: kinds[i],
                    levels: rng.gen_range(5..=12),
                    half_spread_bps: rng.gen_range(2..=8),
                    step_bps: rng.gen_range(1..=4),
                    quote_usd: rng.gen_range(2_000..=20_000) as i128 * USD_SCALE,
                    rng,
                    next_nonce: 0,
                    agent_nonce: 0,
                    cloid_ctr: 0,
                    quotes: vec![Vec::new(); nm],
                    home,
                    partner: None,
                    lev_set: false,
                    agent_approved: false,
                    acted_at: 0,
                    bench_phase: vec![0; nm],
                    resting: Vec::new(),
                }
            })
            .collect();
        for pair in wash.chunks(2) {
            if pair.len() == 2 {
                accts[pair[0]].partner = Some(pair[1]);
                accts[pair[1]].partner = Some(pair[0]);
                accts[pair[1]].home = accts[pair[0]].home;
            }
        }
        let common = feed_common_seed(params.seed);
        let n_val = g.validators.validators.len();
        let max_skew = if n_val >= 4 { (n_val - 1) / 3 } else { 0 };
        let skewed = params.skewed_validators.min(max_skew).min(2);
        let vals = (0..n_val)
            .map(|v| {
                let mut feed = SyntheticFeed::new(&markets, SynthParams::default(), g.genesis_time_ms, common, v as u64 + 1);
                let is_skewed = v >= n_val - skewed;
                if is_skewed {
                    feed.set_skew(if v == n_val - 1 {
                        Skew::Fixed(Rate::from_raw(50_000_000_000)) // +5%
                    } else {
                        Skew::Growing {
                            per_sec: Rate::from_raw(-1_000_000),
                            max: Rate::from_raw(200_000_000_000),
                            start_ms: g.genesis_time_ms,
                        }
                    });
                }
                ValAgent { feed, nonce: 0, skewed: is_skewed, rng: sub_rng(params.seed, "val", v as u64) }
            })
            .collect();
        let mut sp = SynthParams::default();
        sp.noise = Rate::ZERO;
        sp.round_to_valid_px = false;
        let ref_feed = SyntheticFeed::new(&markets, sp, g.genesis_time_ms, common, 0);
        let fair: Vec<Px> = markets.iter().map(|m| m.initial_px).collect();
        let ema: Vec<i128> = fair.iter().map(|p| p.0 as i128).collect();
        let stake = g.validators.validators.first().map(|v| v.stake).unwrap_or(1).max(1);
        Population {
            quorum_count: g.validators.quorum_stake().div_ceil(stake) as usize,
            n_validators: n_val,
            bridge_params: g.bridge.clone(),
            rng: sub_rng(params.seed, "sched", 0),
            mm_acted: vec![0; accts.len() * nm],
            accts,
            by_kind,
            vals,
            ref_feed,
            ema_fast: ema.clone(),
            ema_slow: ema,
            fair,
            markets,
            bridge: BridgeSim::default(),
            acted_now: Vec::new(),
            refresh_cursor: 0,
            watch: BTreeSet::new(),
            height: 0,
            turns: [0; 9],
            actions: [0; 9],
            stats: PopStats::default(),
            params,
        }
    }

    pub fn n_accounts(&self) -> usize {
        self.accts.len()
    }

    pub fn stats(&self) -> PopStats {
        let mut s = self.stats.clone();
        s.turns_by_kind = ALL_KINDS.iter().map(|k| (format!("{k:?}"), self.turns[kind_ix(*k)])).collect();
        s.actions_by_kind = ALL_KINDS.iter().map(|k| (format!("{k:?}"), self.actions[kind_ix(*k)])).collect();
        s
    }

    /// Current fair (reference) price of a market.
    pub fn fair(&self, asset: AssetId) -> Px {
        self.fair[asset as usize]
    }

    // ------------------------------------------------------------ step

    /// Generate the user + validator actions of one block at time `ts`.
    /// `budget` is the number of actions (Σ max(1, order actions)) to aim for.
    pub fn step(&mut self, height: u64, ts: u64, budget: u64, world: &World, keys: &SimKeys) -> Vec<Unsigned> {
        self.height = height;
        self.acted_now.clear();
        for (i, m) in self.markets.iter().enumerate() {
            let r = self.ref_feed.reference_px(m.asset, ts).unwrap_or(m.initial_px);
            self.fair[i] = round_px(m, r.0, false);
            let p = self.fair[i].0 as i128;
            self.ema_fast[i] += (p - self.ema_fast[i]) / 8;
            self.ema_slow[i] += (p - self.ema_slow[i]) / 64;
        }
        let mut out = Vec::new();
        if self.params.validators {
            self.validator_step(height, ts, world, keys, &mut out);
        }
        let weights: [u32; 9] = match self.params.mode {
            Mode::Mixed => [30, 28, 8, 5, 6, 6, 6, 3, 8],
            Mode::Bench => [85, 15, 0, 0, 0, 0, 0, 0, 0],
        };
        let wsum: u32 = (0..9).filter(|i| !self.by_kind[*i].is_empty()).map(|i| weights[i]).sum();
        if wsum == 0 {
            return out;
        }
        let mut used = 0u64;
        let mut tries = 0u64;
        let max_tries = budget * 4 + 64;
        while used < budget && tries < max_tries {
            tries += 1;
            let mut x = self.rng.gen_range(0..wsum);
            let mut kix = 0;
            for i in 0..9 {
                if self.by_kind[i].is_empty() {
                    continue;
                }
                if x < weights[i] {
                    kix = i;
                    break;
                }
                x -= weights[i];
            }
            let list = &self.by_kind[kix];
            let i = list[self.rng.gen_range(0..list.len())];
            let before = out.len();
            self.act(i, ts, world, &mut out);
            if out.len() > before {
                self.turns[kix] += 1;
                if self.accts[i].acted_at != height {
                    self.accts[i].acted_at = height;
                    self.acted_now.push(i);
                }
            }
            for u in &out[before..] {
                let a = u.order_actions().max(1);
                used += a;
                self.actions[kix] += a;
            }
        }
        out
    }

    /// Accounts whose views (first) and open orders (second) the driver
    /// should refresh after executing the block.
    pub fn interest(&mut self) -> (Vec<(usize, Address)>, Vec<(usize, Address)>) {
        let n = self.accts.len();
        let mut set: BTreeSet<usize> = self.acted_now.iter().copied().collect();
        set.extend(self.watch.iter().copied());
        let window = 64.min(n);
        for k in 0..window {
            set.insert((self.refresh_cursor + k) % n.max(1));
        }
        self.refresh_cursor = (self.refresh_cursor + window) % n.max(1);
        let accts = set.iter().map(|i| (*i, self.accts[*i].addr)).collect();
        let mut o: Vec<usize> = self.acted_now.clone();
        o.sort_unstable();
        o.dedup();
        let orders = o.iter().map(|i| (*i, self.accts[*i].addr)).collect();
        (accts, orders)
    }

    /// Absorb refreshed open orders (call after `World::refresh`).
    /// `skip`: sorted accounts whose local view is newer (they acted in a
    /// block generated after the refreshed one).
    pub fn after_block(&mut self, world: &World, skip: &[usize]) {
        if !world.feedback {
            return;
        }
        // watchlist: accounts close to liquidation are refreshed every block
        for (i, a) in world.accounts.iter().enumerate() {
            let Some(a) = a else { continue };
            if a.height != world.height {
                continue;
            }
            if !a.positions.is_empty() && a.maintenance > 0 && a.account_value * 100 < a.maintenance * 200 {
                self.watch.insert(i);
            } else {
                self.watch.remove(&i);
            }
        }
        for (i, oo) in world.open_orders.iter().enumerate() {
            let Some(oo) = oo else { continue };
            if skip.binary_search(&i).is_ok() {
                continue;
            }
            let a = &mut self.accts[i];
            for q in a.quotes.iter_mut() {
                q.clear();
            }
            a.resting.clear();
            for o in oo {
                if o.is_trigger {
                    continue;
                }
                match (a.kind, o.cloid) {
                    (AgentKind::MarketMaker, Some(c)) => {
                        if let Some(q) = a.quotes.get_mut(o.asset as usize) {
                            q.push(Quote { cloid: c, is_buy: o.side == Side::Bid, oid: Some(o.oid) });
                        }
                    }
                    _ => a.resting.push((o.asset, o.oid, o.side == Side::Bid)),
                }
            }
        }
    }

    // ------------------------------------------------------------ helpers

    fn nonce(&mut self, i: usize, ts: u64, agent: bool) -> u64 {
        let a = &mut self.accts[i];
        let slot = if agent { &mut a.agent_nonce } else { &mut a.next_nonce };
        let n = (*slot).max(ts);
        *slot = n + 1;
        n
    }

    fn cloid(&mut self, i: usize) -> Cloid {
        let a = &mut self.accts[i];
        a.cloid_ctr += 1;
        hlwire::cloid_from_u128(((i as u128 + 1) << 64) | a.cloid_ctr as u128)
    }

    fn equity_usd(&self, i: usize, world: &World) -> i128 {
        match world.accounts.get(i).and_then(|x| x.as_ref()) {
            Some(a) if world.feedback => a.account_value,
            _ => genesis_balance_usd(self.accts[i].kind) as i128 * USD_SCALE,
        }
    }

    fn position(&self, i: usize, asset: AssetId, world: &World) -> i64 {
        world.accounts.get(i).and_then(|x| x.as_ref()).map(|a| a.position(asset)).unwrap_or(0)
    }

    fn pick_market(&mut self, i: usize) -> AssetId {
        let nm = self.markets.len() as u32;
        let r = self.accts[i].rng.gen_range(0..100u32);
        if r < 30 || nm == 1 {
            0
        } else if r < 50 {
            1.min(nm - 1)
        } else {
            self.accts[i].rng.gen_range(0..nm)
        }
    }

    fn limit(asset: AssetId, is_buy: bool, px: Px, sz: Sz, tif: Tif, reduce_only: bool, cloid: Option<Cloid>) -> OrderRequest {
        OrderRequest { asset, is_buy, limit_px: px, sz, reduce_only, kind: OrderKind::Limit { tif }, cloid }
    }

    /// Wrap an L1 action into an unsigned tx (own key or approved agent).
    fn l1(&mut self, i: usize, ts: u64, action: Action, use_agent: bool) -> Unsigned {
        let agent = use_agent && self.accts[i].agent_approved;
        let nonce = self.nonce(i, ts, agent);
        let signer = if agent { Signer::Agent(i) } else { Signer::Account(i) };
        let mut u = Unsigned::User { signer, action, nonce, expires_after: None, corruption: Corruption::None, invalid: false };
        self.maybe_corrupt(i, ts, &mut u);
        u
    }

    /// User-signed action whose nonce field equals the tx nonce.
    fn user_signed(&mut self, i: usize, ts: u64, f: impl FnOnce(u64) -> Action) -> Unsigned {
        let nonce = self.nonce(i, ts, false);
        let mut u = Unsigned::User {
            signer: Signer::Account(i),
            action: f(nonce),
            nonce,
            expires_after: None,
            corruption: Corruption::None,
            invalid: false,
        };
        self.maybe_corrupt(i, ts, &mut u);
        u
    }

    fn maybe_corrupt(&mut self, i: usize, ts: u64, u: &mut Unsigned) {
        if self.params.invalid_ppm == 0 || self.accts[i].rng.gen_range(0..1_000_000u32) >= self.params.invalid_ppm {
            return;
        }
        let Unsigned::User { signer, action, nonce, corruption, invalid, .. } = u else { return };
        let user_signed = hlwire::is_user_signed(action);
        let r = self.accts[i].rng.gen_range(0..100u32);
        *invalid = true;
        self.stats.invalid += 1;
        let is_order = matches!(action, Action::Order { .. }) && !user_signed;
        match r {
            0..=19 if !user_signed => {
                // reuse an already used nonce
                *nonce = nonce.saturating_sub(1).max(1);
            }
            0..=24 => {
                // far outside the (time - 2d, time + 1d) window
                *nonce = ts.saturating_sub(3 * 86_400_000);
                if let Some(f) = user_nonce_field(action) {
                    *f = *nonce;
                }
            }
            25..=39 => *corruption = Corruption::BadSignature,
            40..=49 => *signer = Signer::Stranger(ts ^ (i as u64) << 20),
            _ if is_order => {
                let Action::Order { orders, .. } = action else { unreachable!() };
                let o = &mut orders[0];
                match r % 3 {
                    0 => o.limit_px = Px(o.limit_px.0 + 1), // too precise price
                    1 => o.sz = Sz(o.sz.0.saturating_mul(10_000)), // insufficient margin
                    _ => {
                        let spec = &self.markets[o.asset as usize];
                        o.sz = Sz(lot(spec)); // below $10 for most markets
                        if notional_raw(o.limit_px, o.sz) >= 10 * USD_SCALE {
                            o.limit_px = Px(o.limit_px.0 + 1);
                        }
                    }
                }
            }
            _ => match action {
                Action::UsdSend { amount, .. } | Action::Withdraw3 { amount, .. } => {
                    *amount = Usd::from_raw(1_000_000_000 * USD_SCALE) // more than any balance
                }
                Action::UpdateLeverage { leverage, .. } => *leverage = 1000,
                _ => *corruption = Corruption::BadSignature,
            },
        }
    }

    // ------------------------------------------------------------ agents

    fn act(&mut self, i: usize, ts: u64, world: &World, out: &mut Vec<Unsigned>) {
        match self.accts[i].kind {
            AgentKind::MarketMaker => self.act_mm(i, ts, out),
            AgentKind::Taker => self.act_taker(i, ts, world, out),
            AgentKind::Degen => self.act_degen(i, ts, world, out),
            AgentKind::TrendFollower => self.act_trend(i, ts, world, out),
            AgentKind::TpSl => self.act_tpsl(i, ts, world, out),
            AgentKind::Leverage => self.act_leverage(i, ts, world, out),
            AgentKind::Transfer => self.act_transfer(i, ts, out),
            AgentKind::WashTrader => self.act_wash(i, ts, out),
            AgentKind::LiqHunter => self.act_hunter(i, ts, world, out),
        }
    }

    fn ladder_px(&self, i: usize, asset: AssetId, is_buy: bool, level: usize) -> Px {
        let a = &self.accts[i];
        let spec = &self.markets[asset as usize];
        let off = a.half_spread_bps + a.step_bps * level as i64;
        if is_buy {
            px_offset_bps(spec, self.fair[asset as usize], -off, false)
        } else {
            px_offset_bps(spec, self.fair[asset as usize], off, true)
        }
    }

    fn mm_order(&mut self, i: usize, asset: AssetId, is_buy: bool, level: usize, tif: Tif, cloid: Option<Cloid>) -> OrderRequest {
        let px = self.ladder_px(i, asset, is_buy, level);
        let spec = &self.markets[asset as usize];
        let usd = self.accts[i].quote_usd * (10 + level as i128) / 10;
        let sz = order_sz(spec, usd, px);
        Self::limit(asset, is_buy, px, sz, tif, false, cloid)
    }

    fn act_mm(&mut self, i: usize, ts: u64, out: &mut Vec<Unsigned>) {
        let nm = self.markets.len();
        // one turn per (account, market) per block
        let start = self.accts[i].rng.gen_range(0..nm);
        let Some(m) = (0..nm).map(|k| (start + k) % nm).find(|m| self.mm_acted[i * nm + m] != self.height) else { return };
        self.mm_acted[i * nm + m] = self.height;
        let asset = m as AssetId;
        if self.params.mode == Mode::Bench {
            return self.act_mm_bench(i, asset, ts, out);
        }
        let levels = self.accts[i].levels;
        let tif = if self.accts[i].rng.gen_range(0..10) < 8 { Tif::Alo } else { Tif::Gtc };
        let n = self.accts[i].quotes[m].len();
        if n == 0 {
            let mut orders = Vec::with_capacity(2 * levels);
            for l in 0..levels {
                for is_buy in [true, false] {
                    let c = self.cloid(i);
                    orders.push(self.mm_order(i, asset, is_buy, l, tif, Some(c)));
                    self.accts[i].quotes[m].push(Quote { cloid: c, is_buy, oid: None });
                }
            }
            let u = self.l1(i, ts, Action::Order { orders, grouping: Grouping::Na }, false);
            out.push(u);
            return;
        }
        let r = self.accts[i].rng.gen_range(0..100u32);
        let quotes = self.accts[i].quotes[m].clone();
        let key = |q: &Quote| q.oid.map(OidOrCloid::Oid).unwrap_or(OidOrCloid::Cloid(q.cloid));
        if r < 45 {
            // batchModify the whole ladder to the new fair price
            let mut bl = 0;
            let mut al = 0;
            let mut mods = Vec::with_capacity(n.min(50));
            for q in quotes.iter().take(50) {
                let l = if q.is_buy {
                    bl += 1;
                    bl - 1
                } else {
                    al += 1;
                    al - 1
                };
                let o = self.mm_order(i, asset, q.is_buy, l, tif, None);
                mods.push((key(q), o));
            }
            for q in self.accts[i].quotes[m].iter_mut().take(50) {
                q.oid = None; // modify assigns a new oid; cloid is inherited
            }
            let u = self.l1(i, ts, Action::BatchModify { modifies: mods }, false);
            out.push(u);
        } else if r < 60 {
            let k = self.accts[i].rng.gen_range(0..n);
            let q = quotes[k].clone();
            let l = self.accts[i].rng.gen_range(0..levels.min(3));
            let o = self.mm_order(i, asset, q.is_buy, l, tif, None);
            self.accts[i].quotes[m][k].oid = None;
            let u = self.l1(i, ts, Action::Modify { oid: key(&q), order: o }, false);
            out.push(u);
        } else if r < 90 {
            // cancel k quotes (by oid when known, else by cloid), place k fresh ones
            let k = self.accts[i].rng.gen_range(1..=n.min(25));
            let mut by_oid = Vec::new();
            let mut by_cloid = Vec::new();
            let mut sides = Vec::new();
            for _ in 0..k {
                let len = self.accts[i].quotes[m].len();
                let j = self.accts[i].rng.gen_range(0..len);
                let q = self.accts[i].quotes[m].swap_remove(j);
                sides.push(q.is_buy);
                match q.oid {
                    Some(o) => by_oid.push((asset, o)),
                    None => by_cloid.push((asset, q.cloid)),
                }
            }
            if !by_oid.is_empty() {
                let u = self.l1(i, ts, Action::Cancel { cancels: by_oid }, false);
                out.push(u);
            }
            if !by_cloid.is_empty() {
                let u = self.l1(i, ts, Action::CancelByCloid { cancels: by_cloid }, false);
                out.push(u);
            }
            let mut orders = Vec::new();
            for (j, is_buy) in sides.into_iter().enumerate() {
                let c = self.cloid(i);
                orders.push(self.mm_order(i, asset, is_buy, j / 2, tif, Some(c)));
                self.accts[i].quotes[m].push(Quote { cloid: c, is_buy, oid: None });
            }
            let u = self.l1(i, ts, Action::Order { orders, grouping: Grouping::Na }, false);
            out.push(u);
        } else {
            // pull everything (cancel by cloid, batches of 50)
            let all: Vec<(AssetId, Cloid)> = quotes.iter().map(|q| (asset, q.cloid)).collect();
            self.accts[i].quotes[m].clear();
            for c in all.chunks(50) {
                let u = self.l1(i, ts, Action::CancelByCloid { cancels: c.to_vec() }, false);
                out.push(u);
            }
        }
    }

    /// AC3 bench: place 50 -> batchModify 50 -> cancel 50 (by cloid), blind.
    fn act_mm_bench(&mut self, i: usize, asset: AssetId, ts: u64, out: &mut Vec<Unsigned>) {
        let m = asset as usize;
        let phase = self.accts[i].bench_phase[m];
        self.accts[i].bench_phase[m] = (phase + 1) % 3;
        match phase {
            0 => {
                let mut orders = Vec::with_capacity(50);
                self.accts[i].quotes[m].clear();
                for l in 0..25 {
                    for is_buy in [true, false] {
                        let c = self.cloid(i);
                        orders.push(self.mm_order(i, asset, is_buy, l, Tif::Alo, Some(c)));
                        self.accts[i].quotes[m].push(Quote { cloid: c, is_buy, oid: None });
                    }
                }
                let u = self.l1(i, ts, Action::Order { orders, grouping: Grouping::Na }, false);
                out.push(u);
            }
            1 => {
                let quotes = self.accts[i].quotes[m].clone();
                let mut mods = Vec::with_capacity(50);
                for (j, q) in quotes.iter().enumerate() {
                    let o = self.mm_order(i, asset, q.is_buy, j / 2, Tif::Alo, None);
                    mods.push((OidOrCloid::Cloid(q.cloid), o));
                }
                if !mods.is_empty() {
                    let u = self.l1(i, ts, Action::BatchModify { modifies: mods }, false);
                    out.push(u);
                }
            }
            _ => {
                let c: Vec<(AssetId, Cloid)> = self.accts[i].quotes[m].iter().map(|q| (asset, q.cloid)).collect();
                self.accts[i].quotes[m].clear();
                if !c.is_empty() {
                    let u = self.l1(i, ts, Action::CancelByCloid { cancels: c }, false);
                    out.push(u);
                }
            }
        }
    }

    fn ioc(&mut self, _i: usize, asset: AssetId, is_buy: bool, usd: i128, slip_bps: i64, reduce_only: bool) -> OrderRequest {
        let spec = &self.markets[asset as usize];
        let fair = self.fair[asset as usize];
        let px = px_offset_bps(spec, fair, if is_buy { slip_bps } else { -slip_bps }, is_buy);
        let sz = order_sz(spec, usd, fair);
        Self::limit(asset, is_buy, px, sz, Tif::Ioc, reduce_only, None)
    }

    fn act_taker(&mut self, i: usize, ts: u64, world: &World, out: &mut Vec<Unsigned>) {
        let asset = self.pick_market(i);
        let eq = self.equity_usd(i, world).max(0);
        let pos = self.position(i, asset, world);
        let fair = self.fair[asset as usize];
        let pos_ntl = notional_raw(fair, Sz(pos));
        let (is_buy, reduce) = if pos != 0 && (pos_ntl > eq * 3 || self.accts[i].rng.gen_range(0..5) == 0) {
            (pos < 0, true)
        } else {
            (self.accts[i].rng.gen_bool(0.5), false)
        };
        let cap = (eq * 4).max(20 * USD_SCALE);
        let mut usd = heavy_tail_usd(&mut self.accts[i].rng, 12 * USD_SCALE, cap);
        if reduce {
            usd = usd.min(pos_ntl.max(11 * USD_SCALE));
        }
        // "market" orders: 5% slippage like the SDKs; otherwise 0.3-3%
        let slip = if self.accts[i].rng.gen_range(0..5) == 0 { 500 } else { self.accts[i].rng.gen_range(30..300) };
        let mut o = self.ioc(i, asset, is_buy, usd, slip, reduce);
        if reduce {
            let spec = &self.markets[asset as usize];
            o.sz = Sz(o.sz.0.min((pos.abs() / lot(spec)).max(1) * lot(spec)));
        }
        let u = self.l1(i, ts, Action::Order { orders: vec![o], grouping: Grouping::Na }, false);
        out.push(u);
    }

    fn act_degen(&mut self, i: usize, ts: u64, world: &World, out: &mut Vec<Unsigned>) {
        let asset = self.accts[i].home;
        let spec = self.markets[asset as usize].clone();
        if !self.accts[i].lev_set {
            self.accts[i].lev_set = true;
            let a = Action::UpdateLeverage { asset, is_cross: !spec.only_isolated, leverage: spec.max_leverage };
            let u = self.l1(i, ts, a, false);
            out.push(u);
            return;
        }
        let eq = self.equity_usd(i, world);
        let pos = self.position(i, asset, world);
        if pos != 0 && self.accts[i].rng.gen_range(0..10) == 0 {
            let fair = self.fair[asset as usize];
            let mut o = self.ioc(i, asset, pos < 0, notional_raw(fair, Sz(pos)), 200, true);
            o.sz = Sz(pos.abs());
            let u = self.l1(i, ts, Action::Order { orders: vec![o], grouping: Grouping::Na }, false);
            out.push(u);
            return;
        }
        if eq < 15 * USD_SCALE && self.accts[i].rng.gen_range(0..4) != 0 {
            return; // broke: mostly idle until refilled
        }
        let pct = self.accts[i].rng.gen_range(60..98) as i128;
        let usd = (eq.max(USD_SCALE * 20) * spec.max_leverage as i128 * pct / 100).max(12 * USD_SCALE);
        // pile into the existing direction (builds leverage up to the max)
        let is_buy = if pos != 0 && self.accts[i].rng.gen_range(0..100) < 85 { pos > 0 } else { self.accts[i].rng.gen_bool(0.5) };
        let o = self.ioc(i, asset, is_buy, usd, 100, false);
        let u = self.l1(i, ts, Action::Order { orders: vec![o], grouping: Grouping::Na }, false);
        out.push(u);
    }

    fn act_trend(&mut self, i: usize, ts: u64, world: &World, out: &mut Vec<Unsigned>) {
        let asset = self.accts[i].home;
        let m = asset as usize;
        let spec = self.markets[m].clone();
        let (f, s) = (self.ema_fast[m], self.ema_slow[m].max(1));
        let signal_bps = (f - s) * 10_000 / s;
        let pos = self.position(i, asset, world);
        let fair = self.fair[m];
        let cap = 50_000 * USD_SCALE;
        if signal_bps.abs() >= 3 {
            let is_buy = signal_bps > 0;
            let pos_ntl = notional_raw(fair, Sz(pos)) * if pos >= 0 { 1 } else { -1 };
            if (is_buy && pos_ntl > cap) || (!is_buy && pos_ntl < -cap) {
                return;
            }
            let usd = heavy_tail_usd(&mut self.accts[i].rng, 12 * USD_SCALE, 20_000 * USD_SCALE);
            let o = self.ioc(i, asset, is_buy, usd, 40, false);
            let u = self.l1(i, ts, Action::Order { orders: vec![o], grouping: Grouping::Na }, false);
            out.push(u);
        } else {
            // passive: cancel stale resting orders, rest a new GTC limit
            let stale: Vec<(AssetId, Oid)> = self.accts[i].resting.iter().map(|r| (r.0, r.1)).collect();
            self.accts[i].resting.clear();
            if !stale.is_empty() {
                let u = self.l1(i, ts, Action::Cancel { cancels: stale }, false);
                out.push(u);
            }
            let is_buy = self.accts[i].rng.gen_bool(0.5);
            let off = self.accts[i].rng.gen_range(15..60);
            let px = px_offset_bps(&spec, fair, if is_buy { -off } else { off }, !is_buy);
            let usd = heavy_tail_usd(&mut self.accts[i].rng, 12 * USD_SCALE, 10_000 * USD_SCALE);
            let o = Self::limit(asset, is_buy, px, order_sz(&spec, usd, px), Tif::Gtc, false, None);
            let u = self.l1(i, ts, Action::Order { orders: vec![o], grouping: Grouping::Na }, false);
            out.push(u);
        }
    }

    fn trigger(asset: AssetId, is_buy: bool, trigger_px: Px, limit_px: Px, sz: Sz, is_market: bool, tpsl: TpSl) -> OrderRequest {
        OrderRequest {
            asset,
            is_buy,
            limit_px,
            sz,
            reduce_only: true,
            kind: OrderKind::Trigger { trigger_px, is_market, tpsl },
            cloid: None,
        }
    }

    fn act_tpsl(&mut self, i: usize, ts: u64, world: &World, out: &mut Vec<Unsigned>) {
        let asset = self.accts[i].home;
        let m = asset as usize;
        let spec = self.markets[m].clone();
        let fair = self.fair[m];
        let pos = self.position(i, asset, world);
        let r = self.accts[i].rng.gen_range(0..100u32);
        let tp_bps = self.accts[i].rng.gen_range(100..400);
        let sl_bps = self.accts[i].rng.gen_range(50..200);
        if pos == 0 || r < 40 {
            // normalTpsl: entry + TP + SL
            let is_buy = self.accts[i].rng.gen_bool(0.5);
            let usd = heavy_tail_usd(&mut self.accts[i].rng, 15 * USD_SCALE, 20_000 * USD_SCALE);
            let entry_px = px_offset_bps(&spec, fair, if is_buy { 50 } else { -50 }, is_buy);
            let sz = order_sz(&spec, usd, fair);
            let parent = Self::limit(asset, is_buy, entry_px, sz, Tif::Gtc, false, None);
            let dir = if is_buy { 1 } else { -1 };
            let tp_px = px_offset_bps(&spec, fair, dir * tp_bps, is_buy);
            let sl_px = px_offset_bps(&spec, fair, -dir * sl_bps, !is_buy);
            let sl_limit = px_offset_bps(&spec, sl_px, -dir * 500, !is_buy);
            let tp = Self::trigger(asset, !is_buy, tp_px, tp_px, sz, false, TpSl::Tp);
            let sl = Self::trigger(asset, !is_buy, sl_px, sl_limit, sz, true, TpSl::Sl);
            let u = self.l1(i, ts, Action::Order { orders: vec![parent, tp, sl], grouping: Grouping::NormalTpsl }, false);
            out.push(u);
        } else if r < 70 {
            // positionTpsl for the existing position (size follows the position)
            let long = pos > 0;
            let dir = if long { 1 } else { -1 };
            let tp_px = px_offset_bps(&spec, fair, dir * tp_bps, long);
            let sl_px = px_offset_bps(&spec, fair, -dir * sl_bps, !long);
            let sl_limit = px_offset_bps(&spec, sl_px, -dir * 500, !long);
            let tp = Self::trigger(asset, !long, tp_px, tp_px, Sz::ZERO, true, TpSl::Tp);
            let sl = Self::trigger(asset, !long, sl_px, sl_limit, Sz::ZERO, true, TpSl::Sl);
            let u = self.l1(i, ts, Action::Order { orders: vec![tp, sl], grouping: Grouping::PositionTpsl }, false);
            out.push(u);
        } else if r < 85 {
            // standalone stop for the position
            let long = pos > 0;
            let dir = if long { 1 } else { -1 };
            let sl_px = px_offset_bps(&spec, fair, -dir * sl_bps, !long);
            let sl_limit = px_offset_bps(&spec, sl_px, -dir * 500, !long);
            let sl = Self::trigger(asset, !long, sl_px, sl_limit, Sz(pos.abs()), true, TpSl::Sl);
            let u = self.l1(i, ts, Action::Order { orders: vec![sl], grouping: Grouping::Na }, false);
            out.push(u);
        } else {
            // close the position with a market order
            let mut o = self.ioc(i, asset, pos < 0, notional_raw(fair, Sz(pos)), 300, true);
            o.sz = Sz(pos.abs());
            let u = self.l1(i, ts, Action::Order { orders: vec![o], grouping: Grouping::Na }, false);
            out.push(u);
        }
    }

    fn act_leverage(&mut self, i: usize, ts: u64, world: &World, out: &mut Vec<Unsigned>) {
        let r = self.accts[i].rng.gen_range(0..100u32);
        let home = self.accts[i].home;
        let iso_pos = world
            .accounts
            .get(i)
            .and_then(|a| a.as_ref())
            .and_then(|a| a.positions.iter().find(|p| !p.3).map(|p| (p.0, p.1)));
        if r < 35 {
            let asset = self.pick_market(i);
            let spec = &self.markets[asset as usize];
            let is_cross = !spec.only_isolated && self.accts[i].rng.gen_bool(0.6);
            let leverage = self.accts[i].rng.gen_range(1..=spec.max_leverage);
            let u = self.l1(i, ts, Action::UpdateLeverage { asset, is_cross, leverage }, false);
            out.push(u);
        } else if r < 65 {
            if let Some((asset, szi)) = iso_pos {
                let add = self.accts[i].rng.gen_bool(0.6);
                let amt = self.accts[i].rng.gen_range(10..500) as i128 * USD_SCALE;
                let ntli = Usd::from_raw(if add { amt } else { -amt });
                let u = self.l1(i, ts, Action::UpdateIsolatedMargin { asset, is_buy: szi > 0, ntli }, false);
                out.push(u);
            } else {
                let spec = &self.markets[home as usize];
                let lev = 5.min(spec.max_leverage);
                let u = self.l1(i, ts, Action::UpdateLeverage { asset: home, is_cross: false, leverage: lev }, false);
                out.push(u);
                let is_buy = self.accts[i].rng.gen_bool(0.5);
                let o = self.ioc(i, home, is_buy, 2_000 * USD_SCALE, 100, false);
                let u = self.l1(i, ts, Action::Order { orders: vec![o], grouping: Grouping::Na }, false);
                out.push(u);
            }
        } else if r < 80 {
            let time = if self.accts[i].rng.gen_bool(0.7) { Some(ts + self.accts[i].rng.gen_range(6_000..120_000)) } else { None };
            let u = self.l1(i, ts, Action::ScheduleCancel { time }, false);
            out.push(u);
        } else {
            let spec = self.markets[home as usize].clone();
            let is_buy = self.accts[i].rng.gen_bool(0.5);
            let fair = self.fair[home as usize];
            let px = px_offset_bps(&spec, fair, if is_buy { -30 } else { 30 }, !is_buy);
            let o = Self::limit(home, is_buy, px, order_sz(&spec, 500 * USD_SCALE, px), Tif::Gtc, false, None);
            let u = self.l1(i, ts, Action::Order { orders: vec![o], grouping: Grouping::Na }, false);
            out.push(u);
        }
    }

    fn act_transfer(&mut self, i: usize, ts: u64, out: &mut Vec<Unsigned>) {
        let n = self.accts.len();
        let r = self.accts[i].rng.gen_range(0..100u32);
        let dest_ix = self.accts[i].rng.gen_range(0..n);
        let dest = self.accts[dest_ix].addr;
        if !self.accts[i].agent_approved && r < 10 {
            self.accts[i].agent_approved = true;
            let agent = self.accts[i].agent_addr;
            let u = self.user_signed(i, ts, |nonce| Action::ApproveAgent { agent_address: agent, agent_name: None, nonce });
            out.push(u);
            return;
        }
        let u = match r {
            0..=34 => {
                let amount = Usd::from_raw(self.accts[i].rng.gen_range(1..500) as i128 * USD_SCALE);
                self.user_signed(i, ts, |time| Action::UsdSend { destination: dest, amount, time })
            }
            35..=54 => {
                let amount = Usd::from_raw(self.accts[i].rng.gen_range(1..100) as i128 * USD_SCALE);
                let to_perp = self.accts[i].rng.gen_bool(0.5);
                self.user_signed(i, ts, |nonce| Action::UsdClassTransfer { amount, to_perp, nonce })
            }
            55..=69 => {
                let amount = Usd::from_raw(self.accts[i].rng.gen_range(1..100) as i128 * USD_SCALE);
                let token = purr_token_str();
                self.user_signed(i, ts, |time| Action::SpotSend { destination: dest, token, amount, time })
            }
            70..=79 => {
                let amount = Usd::from_raw(self.accts[i].rng.gen_range(5..200) as i128 * USD_SCALE);
                let destination = self.accts[i].addr;
                self.user_signed(i, ts, |time| Action::Withdraw3 { destination, amount, time })
            }
            _ => {
                let asset = self.pick_market(i);
                let is_buy = self.accts[i].rng.gen_bool(0.5);
                let o = self.ioc(i, asset, is_buy, 100 * USD_SCALE, 100, false);
                self.l1(i, ts, Action::Order { orders: vec![o], grouping: Grouping::Na }, true)
            }
        };
        out.push(u);
    }

    fn act_wash(&mut self, i: usize, ts: u64, out: &mut Vec<Unsigned>) {
        let asset = self.accts[i].home;
        let spec = self.markets[asset as usize].clone();
        let fair = self.fair[asset as usize];
        let usd = heavy_tail_usd(&mut self.accts[i].rng, 15 * USD_SCALE, 50_000 * USD_SCALE);
        let sz = order_sz(&spec, usd, fair);
        let is_buy = self.accts[i].rng.gen_bool(0.5);
        let partner = self.accts[i].partner;
        if partner.is_none() || self.accts[i].rng.gen_range(0..10) < 4 {
            // self-cross in one action: resting order then crossing IOC (STP)
            let a = Self::limit(asset, is_buy, fair, sz, Tif::Gtc, false, None);
            let b = Self::limit(asset, !is_buy, fair, sz, Tif::Ioc, false, None);
            let u = self.l1(i, ts, Action::Order { orders: vec![a, b], grouping: Grouping::Na }, false);
            out.push(u);
        } else {
            let p = partner.unwrap();
            let a = Self::limit(asset, is_buy, fair, sz, Tif::Gtc, false, None);
            let u = self.l1(i, ts, Action::Order { orders: vec![a], grouping: Grouping::Na }, false);
            out.push(u);
            let b = Self::limit(asset, !is_buy, fair, sz, Tif::Ioc, false, None);
            let u = self.l1(p, ts, Action::Order { orders: vec![b], grouping: Grouping::Na }, false);
            out.push(u);
            if self.accts[p].acted_at != self.height {
                self.accts[p].acted_at = self.height;
                self.acted_now.push(p);
            }
        }
    }

    fn act_hunter(&mut self, i: usize, ts: u64, world: &World, out: &mut Vec<Unsigned>) {
        // clear leftovers from the previous push
        let stale: Vec<(AssetId, Oid)> = self.accts[i].resting.iter().map(|r| (r.0, r.1)).collect();
        self.accts[i].resting.clear();
        if !stale.is_empty() {
            let u = self.l1(i, ts, Action::Cancel { cancels: stale }, false);
            out.push(u);
        }
        if !world.feedback {
            return;
        }
        // candidates: equity < 125% of maintenance margin
        let mut best: Option<(i128, usize)> = None;
        for (j, a) in world.accounts.iter().enumerate() {
            let Some(a) = a else { continue };
            if j == i || a.positions.is_empty() || a.maintenance <= 0 || a.account_value <= 0 {
                continue;
            }
            if a.account_value * 100 < a.maintenance * 125 {
                let ratio = a.account_value * 1000 / a.maintenance;
                if best.map(|b| ratio < b.0).unwrap_or(true) {
                    best = Some((ratio, j));
                }
            }
        }
        let Some((_, t)) = best else { return };
        let tgt = world.accounts[t].as_ref().unwrap();
        let (asset, szi, _, _) = *tgt.positions.iter().max_by_key(|p| (p.2, std::cmp::Reverse(p.0))).unwrap();
        let push_buy = szi < 0;
        let spec = self.markets[asset as usize].clone();
        let fair = self.fair[asset as usize];
        let eq = self.equity_usd(i, world).max(0);
        let usd = heavy_tail_usd(&mut self.accts[i].rng, 1_000 * USD_SCALE, (eq * 2).max(2_000 * USD_SCALE).min(500_000 * USD_SCALE));
        let sweep = self.ioc(i, asset, push_buy, usd, 300, false);
        let rest_px = px_offset_bps(&spec, fair, if push_buy { 100 } else { -100 }, push_buy);
        let rest = Self::limit(asset, push_buy, rest_px, order_sz(&spec, usd / 4, rest_px), Tif::Gtc, false, None);
        self.stats.liq_pushes += 1;
        let u = self.l1(i, ts, Action::Order { orders: vec![sweep, rest], grouping: Grouping::Na }, false);
        out.push(u);
    }

    // ------------------------------------------------------------ validators

    fn validator_tx(&mut self, v: usize, action: ValidatorAction) -> Unsigned {
        self.vals[v].nonce += 1;
        Unsigned::Validator { validator: v, nonce: self.vals[v].nonce, action }
    }

    fn validator_step(&mut self, height: u64, ts: u64, world: &World, keys: &SimKeys, out: &mut Vec<Unsigned>) {
        // oracle: every validator, every block (block interval <= freshness)
        for v in 0..self.n_validators {
            let prices = interfaces::PriceFeed::prices(&mut self.vals[v].feed, ts);
            let u = self.validator_tx(v, ValidatorAction::OraclePrices { prices });
            out.push(u);
            self.stats.oracle_txs += 1;
        }
        // bridge deposits: honest validators attest within 0-2 blocks;
        // skewed validators skip, conflict, or attest honestly.
        if self.rng.gen_range(0..15) == 0 && !self.accts.is_empty() {
            let id = self.bridge.next_deposit;
            self.bridge.next_deposit += 1;
            let degens = &self.by_kind[kind_ix(AgentKind::Degen)];
            let user_ix = if !degens.is_empty() && self.rng.gen_bool(0.5) {
                degens[self.rng.gen_range(0..degens.len())]
            } else {
                self.rng.gen_range(0..self.accts.len())
            };
            let amount = heavy_tail_usd(&mut self.rng, 10 * USD_SCALE, 50_000 * USD_SCALE);
            let att = DepositAttestation {
                deposit_id: id,
                user: self.accts[user_ix].addr,
                amount: Usd::from_raw(amount),
                eth_tx_hash: hash_borsh(&(self.params.seed, id, "deposit")),
            };
            for v in 0..self.n_validators {
                let r = self.vals[v].rng.gen_range(0..100u32);
                let delay = self.vals[v].rng.gen_range(0..=2u64);
                if self.vals[v].skewed {
                    if r < 50 {
                        continue;
                    }
                    let mut a = att.clone();
                    if r < 75 {
                        a.amount = Usd::from_raw(a.amount.raw() * 2);
                    }
                    self.bridge.scheduled.push((height + delay, v, a));
                } else {
                    self.bridge.scheduled.push((height + delay, v, att.clone()));
                    if r < 3 {
                        // duplicate attestation (watchers resubmit): idempotent
                        self.bridge.scheduled.push((height + delay + 1, v, att.clone()));
                    }
                }
            }
        }
        let mut due = Vec::new();
        self.bridge.scheduled.retain(|(h, v, a)| {
            if *h <= height {
                due.push((*v, a.clone()));
                false
            } else {
                true
            }
        });
        for (v, a) in due {
            let u = self.validator_tx(v, ValidatorAction::BridgeDeposit(a));
            out.push(u);
            self.stats.deposit_attestations += 1;
        }
        // withdrawals: sign pending ones, then finalize once a quorum signed
        for w in &world.pending_withdrawals {
            for v in 0..self.n_validators {
                if !self.bridge.signed.contains(&(w.id, v)) && self.vals[v].rng.gen_range(0..10) < 7 {
                    self.bridge.signed.insert((w.id, v));
                    let mut d = relayer::withdrawal_digest(w, &self.bridge_params);
                    if self.vals[v].skewed && self.vals[v].rng.gen_range(0..10) < 3 {
                        d[0] ^= 0xff; // garbage signature (rejected by the chain)
                    }
                    let signature = keys.validators[v].sign_digest(&d);
                    let u = self.validator_tx(v, ValidatorAction::WithdrawalSignature { withdrawal_id: w.id, signature });
                    out.push(u);
                    self.stats.withdrawal_sigs += 1;
                }
            }
            if w.signatures.len() >= self.quorum_count {
                for v in 0..self.n_validators {
                    if !self.bridge.finalized.contains(&(w.id, v)) && self.vals[v].rng.gen_range(0..4) == 0 {
                        self.bridge.finalized.insert((w.id, v));
                        let u = self.validator_tx(v, ValidatorAction::WithdrawalFinalized { withdrawal_id: w.id });
                        out.push(u);
                        self.stats.withdrawal_finalized += 1;
                    }
                }
            }
        }
    }

}

fn user_nonce_field(a: &mut Action) -> Option<&mut u64> {
    match a {
        Action::UsdSend { time, .. } | Action::SpotSend { time, .. } | Action::Withdraw3 { time, .. } => Some(time),
        Action::UsdClassTransfer { nonce, .. } | Action::ApproveAgent { nonce, .. } => Some(nonce),
        _ => None,
    }
}
