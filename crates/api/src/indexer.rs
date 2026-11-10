//! History indexer: consumes executed [`BlockOutcome`]s and keeps bounded
//! per-user / per-coin history for `/info` and WebSocket snapshots.

use crate::candle::{Interval, ALL_INTERVALS};
use interfaces::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};

/// Memory bounds.
#[derive(Clone, Debug)]
pub struct IndexerLimits {
    pub user_fills: usize,
    pub user_orders: usize,
    pub user_funding: usize,
    pub user_ledger: usize,
    /// Global cap on stored per-user records (fills+orders+funding+ledger);
    /// least-recently-active users are evicted beyond it.
    pub max_records: usize,
    pub max_users: usize,
    pub trades_per_coin: usize,
    pub candles_per_series: usize,
    pub funding_points_per_coin: usize,
}

impl Default for IndexerLimits {
    fn default() -> Self {
        Self {
            user_fills: 2000,
            user_orders: 2000,
            user_funding: 1000,
            user_ledger: 1000,
            max_records: 2_000_000,
            max_users: 200_000,
            trades_per_coin: 1000,
            candles_per_series: 5000,
            funding_points_per_coin: 10_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FillRec {
    pub asset: AssetId,
    pub px: Px,
    /// Always positive.
    pub sz: Sz,
    pub side: Side,
    pub time: TimestampMs,
    pub start_pos: Sz,
    pub closed_pnl: Usd,
    pub hash: TxHash,
    pub oid: Oid,
    pub crossed: bool,
    pub fee: Usd,
    pub tid: Tid,
    pub cloid: Option<Cloid>,
    pub liquidation: Option<LiquidationInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderRec {
    pub order: RestingOrderView,
    pub status: OrderLifecycle,
    pub status_ts: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FundingRec {
    pub time: TimestampMs,
    pub asset: AssetId,
    pub usdc: Usd,
    pub szi: Sz,
    pub rate: Rate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerRec {
    pub time: TimestampMs,
    pub hash: TxHash,
    pub delta: LedgerDelta,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TradeRec {
    pub asset: AssetId,
    /// Taker side.
    pub side: Side,
    pub px: Px,
    pub sz: Sz,
    pub time: TimestampMs,
    pub hash: TxHash,
    pub tid: Tid,
    pub buyer: Address,
    pub seller: Address,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candle {
    pub t: u64,
    pub close_t: u64,
    pub o: Px,
    pub c: Px,
    pub h: Px,
    pub l: Px,
    /// Base volume, raw 1e-8 units.
    pub v: i128,
    /// Quote (USD) notional volume, raw 1e-6 units.
    pub ntl: i128,
    pub n: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FundingPoint {
    pub time: TimestampMs,
    pub rate: Rate,
    pub premium: Rate,
}

#[derive(Default)]
struct UserHist {
    fills: VecDeque<FillRec>,
    orders: FastMap<Oid, OrderRec>,
    order_seq: VecDeque<Oid>,
    cloids: FastMap<Cloid, Oid>,
    funding: VecDeque<FundingRec>,
    ledger: VecDeque<LedgerRec>,
    /// Cumulative traded notional (USD raw 1e-6).
    cum_vlm: i128,
    n_requests: u64,
    lru: u64,
}

impl UserHist {
    fn records(&self) -> usize {
        self.fills.len() + self.order_seq.len() + self.funding.len() + self.ledger.len()
    }
}

/// What changed for whom in one block (used for WS pushes). Built from the
/// block only when the WS hub has subscribers ([`BlockDelta::build`]).
#[derive(Default, Debug)]
pub struct BlockDelta {
    pub height: Height,
    pub fills_by_user: HashMap<Address, Vec<FillRec>>,
    pub trades_by_asset: BTreeMap<AssetId, Vec<TradeRec>>,
    pub orders_by_user: HashMap<Address, Vec<OrderRec>>,
    pub funding_by_user: HashMap<Address, Vec<FundingRec>>,
    pub ledger_by_user: HashMap<Address, Vec<LedgerRec>>,
    pub book_changed: HashSet<AssetId>,
    /// Users touched in any way (fills, orders, funding, ledger, receipts).
    pub touched: HashSet<Address>,
    /// Candle (asset, interval) series updated by this block.
    pub candles_changed: HashSet<(AssetId, Interval)>,
}

/// Cheap non-cryptographic hasher for the indexer's maps (keys are
/// addresses, oids and cloids; the indexer is not consensus state).
#[derive(Default, Clone, Copy)]
pub struct FastHasher(u64);
impl Hasher for FastHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.write_u64(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let r = chunks.remainder();
        if !r.is_empty() {
            let mut b = [0u8; 8];
            b[..r.len()].copy_from_slice(r);
            self.write_u64(u64::from_le_bytes(b));
        }
    }
    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64)
    }
    #[inline]
    fn write_u32(&mut self, n: u32) {
        self.write_u64(n as u64)
    }
    #[inline]
    fn finish(&self) -> u64 {
        // final avalanche so low bits are usable by hashbrown
        let mut x = self.0;
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
        x ^ (x >> 33)
    }
}
pub type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FastHasher>>;

const N_IV: usize = ALL_INTERVALS.len();

pub struct Indexer {
    limits: IndexerLimits,
    users: FastMap<Address, UserHist>,
    /// (block sequence of last activity, user): least recently active first.
    lru: BTreeSet<(u64, Address)>,
    /// Incremented once per ingested block.
    seq: u64,
    total_records: usize,
    trades: FastMap<AssetId, VecDeque<TradeRec>>,
    candles: FastMap<AssetId, Box<[VecDeque<Candle>; N_IV]>>,
    funding_hist: FastMap<AssetId, VecDeque<FundingPoint>>,
    /// Exchange-wide traded notional per UTC day (day number -> USD raw).
    daily_exchange: BTreeMap<u64, i128>,
    height: Height,
    last_event_time: TimestampMs,
}

fn notional(px: Px, sz: Sz) -> i128 {
    // px 1e-8 * sz 1e-8 = 1e-16 -> USD 1e-6
    mul_div(px.raw(), sz.raw().abs(), 10_000_000_000, Rounding::TowardZero)
}

fn trade_rec(f: &FillEvent) -> TradeRec {
    let (buyer, seller) = if f.taker_side.is_buy() { (f.taker, f.maker) } else { (f.maker, f.taker) };
    TradeRec { asset: f.asset, side: f.taker_side, px: f.px, sz: f.sz.abs(), time: f.time, hash: f.tx_hash, tid: f.tid, buyer, seller }
}

/// (taker, taker rec), (maker, maker rec).
fn fill_recs(f: &FillEvent) -> [(Address, FillRec); 2] {
    let sz = f.sz.abs();
    let taker = FillRec {
        asset: f.asset,
        px: f.px,
        sz,
        side: f.taker_side,
        time: f.time,
        start_pos: f.taker_start_pos,
        closed_pnl: f.taker_closed_pnl,
        hash: f.tx_hash,
        oid: f.taker_oid,
        crossed: true,
        fee: f.taker_fee,
        tid: f.tid,
        cloid: f.taker_cloid,
        liquidation: f.liquidation.clone().filter(|l| l.liquidated_user == f.taker),
    };
    let maker = FillRec {
        asset: f.asset,
        px: f.px,
        sz,
        side: f.taker_side.opposite(),
        time: f.time,
        start_pos: f.maker_start_pos,
        closed_pnl: f.maker_closed_pnl,
        hash: f.tx_hash,
        oid: f.maker_oid,
        crossed: false,
        fee: f.maker_fee,
        tid: f.tid,
        cloid: f.maker_cloid,
        liquidation: f.liquidation.clone().filter(|l| l.liquidated_user == f.maker),
    };
    [(f.taker, taker), (f.maker, maker)]
}

/// Stored order view: "Limit" order types are kept as an empty string (no
/// allocation); rendering maps anything but "Market" to "Limit". `buf` is a
/// previous value whose allocation may be reused.
fn stored_order(o: &RestingOrderView, buf: String) -> RestingOrderView {
    let order_type = if matches!(o.order_type.as_str(), "Market" | "market") {
        if buf == "Market" {
            buf
        } else {
            "Market".to_string()
        }
    } else {
        String::new()
    };
    RestingOrderView {
        asset: o.asset,
        oid: o.oid,
        side: o.side,
        limit_px: o.limit_px,
        sz: o.sz,
        orig_sz: o.orig_sz,
        timestamp: o.timestamp,
        cloid: o.cloid,
        reduce_only: o.reduce_only,
        order_type,
        tif: o.tif,
        trigger: o.trigger,
        is_trigger: o.is_trigger,
    }
}

/// Ledger records per user, including the receiving side of transfers
/// unless the state machine emitted a record for it itself.
fn ledger_records(b: &BlockOutcome, mut f: impl FnMut(Address, LedgerRec)) {
    let mut seen: HashSet<(Address, TxHash, u8)> = HashSet::new();
    let mut any = false;
    for e in &b.events {
        if let Event::Ledger { user, tx_hash, delta, .. } = e {
            seen.insert((*user, *tx_hash, ledger_kind(delta)));
            any = true;
        }
    }
    if !any {
        return;
    }
    for e in &b.events {
        if let Event::Ledger { user, time, tx_hash, delta } = e {
            let rec = LedgerRec { time: *time, hash: *tx_hash, delta: delta.clone() };
            let dest = match delta {
                LedgerDelta::InternalTransfer { destination, .. } => Some(*destination),
                LedgerDelta::SpotTransfer { destination, .. } => Some(*destination),
                _ => None,
            };
            let second = dest.filter(|d| *d != *user && seen.insert((*d, *tx_hash, ledger_kind(delta))));
            match second {
                Some(d) => {
                    f(*user, rec.clone());
                    f(d, rec);
                }
                None => f(*user, rec),
            }
        }
    }
}

impl BlockDelta {
    /// Group a block's events per user / asset for WS pushes.
    pub fn build(b: &BlockOutcome) -> Self {
        let mut d = BlockDelta { height: b.height, ..Default::default() };
        for r in &b.receipts {
            if r.user != Address::ZERO {
                d.touched.insert(r.user);
            }
        }
        for e in &b.events {
            match e {
                Event::Fill(f) => {
                    d.trades_by_asset.entry(f.asset).or_default().push(trade_rec(f));
                    for (who, rec) in fill_recs(f) {
                        d.fills_by_user.entry(who).or_default().push(rec);
                        d.touched.insert(who);
                    }
                    for iv in ALL_INTERVALS {
                        d.candles_changed.insert((f.asset, iv));
                    }
                }
                Event::OrderUpdate { user, order, status, time } => {
                    d.orders_by_user.entry(*user).or_default().push(OrderRec { order: order.clone(), status: *status, status_ts: *time });
                    d.touched.insert(*user);
                }
                Event::Funding { user, asset, usdc, szi, funding_rate, time } => {
                    let rec = FundingRec { time: *time, asset: *asset, usdc: *usdc, szi: *szi, rate: *funding_rate };
                    d.funding_by_user.entry(*user).or_default().push(rec);
                    d.touched.insert(*user);
                }
                Event::BookChanged { asset } => {
                    d.book_changed.insert(*asset);
                }
                Event::Ledger { .. } | Event::Adl { .. } | Event::BadDebtCovered { .. } => {}
            }
        }
        ledger_records(b, |u, rec| {
            d.ledger_by_user.entry(u).or_default().push(rec);
            d.touched.insert(u);
        });
        d
    }
}

impl Default for Indexer {
    fn default() -> Self {
        Self::new(IndexerLimits::default())
    }
}

impl Indexer {
    pub fn new(limits: IndexerLimits) -> Self {
        Self {
            limits,
            users: FastMap::default(),
            lru: BTreeSet::new(),
            seq: 0,
            total_records: 0,
            trades: FastMap::default(),
            candles: FastMap::default(),
            funding_hist: FastMap::default(),
            daily_exchange: BTreeMap::new(),
            height: 0,
            last_event_time: 0,
        }
    }

    pub fn height(&self) -> Height {
        self.height
    }
    pub fn last_event_time(&self) -> TimestampMs {
        self.last_event_time
    }
    pub fn total_records(&self) -> usize {
        self.total_records
    }
    pub fn n_users(&self) -> usize {
        self.users.len()
    }

    /// User history, marking it active in this block (LRU touched at most
    /// once per user per block).
    #[inline]
    fn user<'a>(users: &'a mut FastMap<Address, UserHist>, lru: &mut BTreeSet<(u64, Address)>, seq: u64, a: Address) -> &'a mut UserHist {
        let u = users.entry(a).or_default();
        if u.lru != seq {
            if u.lru != 0 {
                lru.remove(&(u.lru, a));
            }
            u.lru = seq;
            lru.insert((seq, a));
        }
        u
    }

    /// Index one executed block. `premium(asset)` supplies the current premium
    /// for funding history points. WS deltas are built separately
    /// ([`BlockDelta::build`]) and only when someone is subscribed.
    pub fn ingest(&mut self, b: &BlockOutcome, premium: &dyn Fn(AssetId) -> Rate) {
        self.height = b.height;
        self.seq += 1;
        let seq = self.seq;
        let lim = &self.limits;
        let users = &mut self.users;
        let lru = &mut self.lru;
        let mut total = self.total_records;
        let mut last_t = self.last_event_time;
        for r in &b.receipts {
            if r.user != Address::ZERO {
                Self::user(users, lru, seq, r.user).n_requests += 1;
            }
        }
        // cache of the last day bucket for daily_exchange
        let mut day_acc: Option<(u64, i128)> = None;
        for e in &b.events {
            match e {
                Event::Fill(f) => {
                    last_t = last_t.max(f.time);
                    let tq = self.trades.entry(f.asset).or_default();
                    tq.push_back(trade_rec(f));
                    if tq.len() > lim.trades_per_coin {
                        tq.pop_front();
                    }
                    let sz = f.sz.abs();
                    let ntl = notional(f.px, sz);
                    let series = self.candles.entry(f.asset).or_insert_with(|| Box::new(std::array::from_fn(|_| VecDeque::new())));
                    for (i, iv) in ALL_INTERVALS.iter().enumerate() {
                        let q = &mut series[i];
                        match q.back_mut() {
                            Some(c) if c.t <= f.time && f.time <= c.close_t => {
                                c.c = f.px;
                                c.h = c.h.max(f.px);
                                c.l = c.l.min(f.px);
                                c.v += sz.raw();
                                c.ntl += ntl;
                                c.n += 1;
                            }
                            Some(c) if c.t > f.time => {
                                // out-of-order fill (should not happen): fold into the latest candle
                                c.v += sz.raw();
                                c.ntl += ntl;
                                c.n += 1;
                            }
                            _ => {
                                let (t, close_t) = iv.bucket(f.time);
                                q.push_back(Candle { t, close_t, o: f.px, c: f.px, h: f.px, l: f.px, v: sz.raw(), ntl, n: 1 });
                                if q.len() > lim.candles_per_series {
                                    q.pop_front();
                                }
                            }
                        }
                    }
                    let day = f.time / 86_400_000;
                    match &mut day_acc {
                        Some((d, acc)) if *d == day => *acc += ntl,
                        _ => {
                            if let Some((d, acc)) = day_acc.take() {
                                *self.daily_exchange.entry(d).or_default() += acc;
                            }
                            day_acc = Some((day, ntl));
                        }
                    }
                    for (who, rec) in fill_recs(f) {
                        let u = Self::user(users, lru, seq, who);
                        u.cum_vlm += ntl;
                        u.fills.push_back(rec);
                        if u.fills.len() > lim.user_fills {
                            u.fills.pop_front();
                        } else {
                            total += 1;
                        }
                    }
                }
                Event::OrderUpdate { user, order, status, time } => {
                    last_t = last_t.max(*time);
                    let u = Self::user(users, lru, seq, *user);
                    let oid = order.oid;
                    match u.orders.get_mut(&oid) {
                        Some(existing) => {
                            // Keep the original order description (orig size,
                            // placement time) but take the latest remaining
                            // size and status.
                            let orig_sz = existing.order.orig_sz;
                            let buf = std::mem::take(&mut existing.order.order_type);
                            existing.order = stored_order(order, buf);
                            if existing.order.orig_sz.is_zero() {
                                existing.order.orig_sz = orig_sz;
                            }
                            existing.status = *status;
                            existing.status_ts = *time;
                        }
                        None => {
                            u.orders.insert(oid, OrderRec { order: stored_order(order, String::new()), status: *status, status_ts: *time });
                            u.order_seq.push_back(oid);
                            total += 1;
                            while u.order_seq.len() > lim.user_orders {
                                if let Some(old) = u.order_seq.pop_front() {
                                    if let Some(o) = u.orders.remove(&old) {
                                        if let Some(c) = o.order.cloid {
                                            if u.cloids.get(&c) == Some(&old) {
                                                u.cloids.remove(&c);
                                            }
                                        }
                                    }
                                    total -= 1;
                                }
                            }
                        }
                    }
                    if let Some(c) = order.cloid {
                        if u.orders.contains_key(&oid) {
                            u.cloids.insert(c, oid);
                        }
                    }
                }
                Event::Funding { user, asset, usdc, szi, funding_rate, time } => {
                    last_t = last_t.max(*time);
                    let u = Self::user(users, lru, seq, *user);
                    u.funding.push_back(FundingRec { time: *time, asset: *asset, usdc: *usdc, szi: *szi, rate: *funding_rate });
                    if u.funding.len() > lim.user_funding {
                        u.funding.pop_front();
                    } else {
                        total += 1;
                    }
                    let fh = self.funding_hist.entry(*asset).or_default();
                    if fh.back().map(|p| p.time) != Some(*time) {
                        fh.push_back(FundingPoint { time: *time, rate: *funding_rate, premium: premium(*asset) });
                        if fh.len() > lim.funding_points_per_coin {
                            fh.pop_front();
                        }
                    }
                }
                Event::Ledger { time, .. } => last_t = last_t.max(*time),
                Event::BookChanged { .. } | Event::Adl { .. } | Event::BadDebtCovered { .. } => {}
            }
        }
        if let Some((d, acc)) = day_acc {
            *self.daily_exchange.entry(d).or_default() += acc;
        }
        while self.daily_exchange.len() > 60 {
            self.daily_exchange.pop_first();
        }
        ledger_records(b, |who, rec| {
            let u = Self::user(users, lru, seq, who);
            u.ledger.push_back(rec);
            if u.ledger.len() > lim.user_ledger {
                u.ledger.pop_front();
            } else {
                total += 1;
            }
        });
        self.total_records = total;
        self.last_event_time = last_t;
        self.enforce_global();
    }

    fn enforce_global(&mut self) {
        while self.total_records > self.limits.max_records || self.users.len() > self.limits.max_users {
            let Some((_, a)) = self.lru.pop_first() else { break };
            if let Some(u) = self.users.remove(&a) {
                self.total_records -= u.records();
            }
        }
    }

    fn series(&self, asset: AssetId, iv: Interval) -> Option<&VecDeque<Candle>> {
        self.candles.get(&asset).map(|s| &s[iv as usize])
    }

    // ------------------------------------------------------------ queries ---

    /// Most recent first.
    pub fn user_fills(&self, user: &Address, limit: usize) -> Vec<FillRec> {
        self.users.get(user).map(|u| u.fills.iter().rev().take(limit).cloned().collect()).unwrap_or_default()
    }

    /// Ascending by time, `start <= time <= end`.
    pub fn user_fills_by_time(&self, user: &Address, start: u64, end: u64, limit: usize) -> Vec<FillRec> {
        self.users
            .get(user)
            .map(|u| u.fills.iter().filter(|f| f.time >= start && f.time <= end).take(limit).cloned().collect())
            .unwrap_or_default()
    }

    pub fn order(&self, user: &Address, oid: Oid) -> Option<OrderRec> {
        self.users.get(user)?.orders.get(&oid).cloned()
    }

    pub fn order_by_cloid(&self, user: &Address, cloid: &Cloid) -> Option<OrderRec> {
        let u = self.users.get(user)?;
        u.orders.get(u.cloids.get(cloid)?).cloned()
    }

    /// Most recent first.
    pub fn historical_orders(&self, user: &Address, limit: usize) -> Vec<OrderRec> {
        let Some(u) = self.users.get(user) else { return vec![] };
        let mut v: Vec<OrderRec> = u.orders.values().cloned().collect();
        v.sort_by(|a, b| (b.status_ts, b.order.oid).cmp(&(a.status_ts, a.order.oid)));
        v.truncate(limit);
        v
    }

    pub fn user_funding(&self, user: &Address, start: u64, end: u64) -> Vec<FundingRec> {
        self.users
            .get(user)
            .map(|u| u.funding.iter().filter(|f| f.time >= start && f.time <= end).cloned().collect())
            .unwrap_or_default()
    }

    pub fn user_ledger(&self, user: &Address, start: u64, end: u64) -> Vec<LedgerRec> {
        self.users
            .get(user)
            .map(|u| u.ledger.iter().filter(|f| f.time >= start && f.time <= end).cloned().collect())
            .unwrap_or_default()
    }

    /// Most recent first.
    pub fn recent_trades(&self, asset: AssetId, limit: usize) -> Vec<TradeRec> {
        self.trades.get(&asset).map(|q| q.iter().rev().take(limit).cloned().collect()).unwrap_or_default()
    }

    /// Ascending, candles whose open time is within [start, end].
    pub fn candles(&self, asset: AssetId, iv: Interval, start: u64, end: u64) -> Vec<Candle> {
        self.series(asset, iv)
            .map(|q| q.iter().filter(|c| c.close_t >= start && c.t <= end).cloned().collect())
            .unwrap_or_default()
    }

    pub fn last_candle(&self, asset: AssetId, iv: Interval) -> Option<Candle> {
        self.series(asset, iv).and_then(|q| q.back().cloned())
    }

    pub fn funding_history(&self, asset: AssetId, start: u64, end: u64) -> Vec<FundingPoint> {
        self.funding_hist
            .get(&asset)
            .map(|q| q.iter().filter(|p| p.time >= start && p.time <= end).copied().collect())
            .unwrap_or_default()
    }

    pub fn cum_vlm(&self, user: &Address) -> i128 {
        self.users.get(user).map(|u| u.cum_vlm).unwrap_or(0)
    }

    pub fn n_requests(&self, user: &Address) -> u64 {
        self.users.get(user).map(|u| u.n_requests).unwrap_or(0)
    }

    /// 24h base volume (raw 1e-8) ending at `now`, from 1h candles.
    pub fn day_base_vlm(&self, asset: AssetId, now: u64) -> i128 {
        let from = now.saturating_sub(24 * 3_600_000);
        self.series(asset, Interval::H1)
            .map(|q| q.iter().rev().take_while(|c| c.close_t >= from).map(|c| c.v).sum())
            .unwrap_or(0)
    }

    /// Quote notional traded in the last 24 h (1h candles), USD raw.
    pub fn day_ntl_vlm(&self, asset: AssetId, now: u64) -> i128 {
        let from = now.saturating_sub(24 * 3_600_000);
        self.series(asset, Interval::H1)
            .map(|q| q.iter().rev().take_while(|c| c.close_t >= from).map(|c| c.ntl).sum())
            .unwrap_or(0)
    }

    /// Price 24 h ago: close of the last 1h candle that ended before the
    /// window, else the open of the oldest candle inside it.
    pub fn prev_day_px(&self, asset: AssetId, now: u64) -> Option<Px> {
        let from = now.saturating_sub(24 * 3_600_000);
        let q = self.series(asset, Interval::H1)?;
        if let Some(c) = q.iter().rev().find(|c| c.close_t < from) {
            return Some(c.c);
        }
        q.iter().find(|c| c.close_t >= from).map(|c| c.o)
    }

    /// Per-day (date, taker notional, maker notional, exchange notional) USD
    /// raw, ascending, for the last `days` days with exchange activity.
    pub fn daily_volumes(&self, user: &Address, days: usize) -> Vec<(String, i128, i128, i128)> {
        let mut m: BTreeMap<u64, (i128, i128)> = BTreeMap::new();
        for d in self.daily_exchange.keys().rev().take(days) {
            m.insert(*d, (0, 0));
        }
        let first = m.keys().next().copied().unwrap_or(u64::MAX);
        let fills = self.users.get(user).map(|u| u.fills.iter().collect::<Vec<_>>()).unwrap_or_default();
        for f in fills {
            let day = f.time / 86_400_000;
            if day < first {
                continue;
            }
            let e = m.entry(day).or_default();
            let n = notional(f.px, f.sz);
            if f.crossed {
                e.0 += n
            } else {
                e.1 += n
            }
        }
        m.into_iter()
            .map(|(k, (a, b))| {
                (crate::candle::date_str(k * 86_400_000), a, b, self.daily_exchange.get(&k).copied().unwrap_or(0))
            })
            .collect()
    }
}

fn ledger_kind(d: &LedgerDelta) -> u8 {
    match d {
        LedgerDelta::Deposit { .. } => 0,
        LedgerDelta::Withdraw { .. } => 1,
        LedgerDelta::InternalTransfer { .. } => 2,
        LedgerDelta::SpotTransfer { .. } => 3,
        LedgerDelta::AccountClassTransfer { .. } => 4,
        LedgerDelta::Liquidation { .. } => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(n: u32) -> Address {
        let mut x = [0u8; 20];
        x[..4].copy_from_slice(&n.to_le_bytes());
        Address(x)
    }

    fn order_ev(user: Address, oid: Oid, status: OrderLifecycle) -> Event {
        Event::OrderUpdate {
            user,
            order: RestingOrderView {
                asset: 0,
                oid,
                side: Side::Bid,
                limit_px: Px::parse("100").unwrap(),
                sz: Sz::parse("1").unwrap(),
                orig_sz: Sz::parse("1").unwrap(),
                timestamp: 1,
                cloid: Some([oid as u8; 16]),
                reduce_only: false,
                order_type: "Limit".into(),
                tif: Some(Tif::Gtc),
                trigger: None,
                is_trigger: false,
            },
            status,
            time: oid,
        }
    }

    fn block(h: Height, events: Vec<Event>) -> BlockOutcome {
        BlockOutcome { height: h, state_root: Hash32::ZERO, receipts: vec![], events, order_actions: 0 }
    }

    fn recount(ix: &Indexer) -> usize {
        ix.users.values().map(|u| u.records()).sum()
    }

    #[test]
    fn bounded_and_lru_once_per_block() {
        let lim = IndexerLimits { user_orders: 5, max_records: 40, max_users: 6, ..Default::default() };
        let mut ix = Indexer::new(lim);
        let mut oid = 0;
        for h in 1..=50u64 {
            let mut ev = vec![];
            for u in 0..3 {
                for _ in 0..4 {
                    oid += 1;
                    ev.push(order_ev(a((h as u32 * 3 + u) % 10), oid, OrderLifecycle::Open));
                    ev.push(order_ev(a((h as u32 * 3 + u) % 10), oid, OrderLifecycle::Filled));
                }
            }
            ix.ingest(&block(h, ev), &|_| Rate::ZERO);
            assert!(ix.n_users() <= 6);
            assert!(ix.total_records() <= 40);
            assert_eq!(ix.total_records(), recount(&ix));
            assert_eq!(ix.lru.len(), ix.n_users());
            for u in ix.users.values() {
                assert!(u.order_seq.len() <= 5);
                assert_eq!(u.orders.len(), u.order_seq.len());
                assert!(u.cloids.values().all(|o| u.orders.contains_key(o)));
            }
        }
        // the most recently active user survived, with the latest status
        let last = a((50 * 3 + 2) % 10);
        let r = ix.order(&last, oid).unwrap();
        assert_eq!(r.status, OrderLifecycle::Filled);
        assert_eq!(r.order.order_type, ""); // stored compactly, rendered as "Limit"
        assert_eq!(ix.order_by_cloid(&last, &[oid as u8; 16]).unwrap().order.oid, oid);
    }

    #[test]
    fn delta_matches_block() {
        let u = a(1);
        let b = block(7, vec![order_ev(u, 1, OrderLifecycle::Open), Event::BookChanged { asset: 3 }]);
        let d = BlockDelta::build(&b);
        assert_eq!(d.height, 7);
        assert_eq!(d.orders_by_user[&u].len(), 1);
        assert_eq!(d.orders_by_user[&u][0].order.order_type, "Limit"); // deltas carry the original view
        assert!(d.book_changed.contains(&3));
        assert!(d.touched.contains(&u));
    }
}
