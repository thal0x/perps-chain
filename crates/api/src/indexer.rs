//! History indexer: consumes executed [`BlockOutcome`]s and keeps bounded
//! per-user / per-coin history for `/info` and WebSocket snapshots.

use crate::candle::{Interval, ALL_INTERVALS};
use interfaces::*;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

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
    orders: HashMap<Oid, OrderRec>,
    order_seq: VecDeque<Oid>,
    cloids: HashMap<Cloid, Oid>,
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

/// What changed for whom in one block (used for WS pushes).
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

pub struct Indexer {
    limits: IndexerLimits,
    users: HashMap<Address, UserHist>,
    lru: BTreeMap<u64, Address>,
    seq: u64,
    total_records: usize,
    trades: HashMap<AssetId, VecDeque<TradeRec>>,
    candles: HashMap<(AssetId, Interval), VecDeque<Candle>>,
    funding_hist: HashMap<AssetId, VecDeque<FundingPoint>>,
    /// Exchange-wide traded notional per UTC day (day number -> USD raw).
    daily_exchange: BTreeMap<u64, i128>,
    height: Height,
    last_event_time: TimestampMs,
}

fn notional(px: Px, sz: Sz) -> i128 {
    // px 1e-8 * sz 1e-8 = 1e-16 -> USD 1e-6
    mul_div(px.raw(), sz.raw().abs(), 10_000_000_000, Rounding::TowardZero)
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
            users: HashMap::new(),
            lru: BTreeMap::new(),
            seq: 0,
            total_records: 0,
            trades: HashMap::new(),
            candles: HashMap::new(),
            funding_hist: HashMap::new(),
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

    fn user(&mut self, a: Address) -> &mut UserHist {
        self.seq += 1;
        let seq = self.seq;
        let u = self.users.entry(a).or_default();
        if u.lru != 0 {
            self.lru.remove(&u.lru);
        }
        u.lru = seq;
        self.lru.insert(seq, a);
        u
    }

    /// Index one executed block. `premium(asset)` supplies the current premium
    /// for funding history points.
    pub fn ingest(&mut self, b: &BlockOutcome, premium: &dyn Fn(AssetId) -> Rate) -> BlockDelta {
        let mut d = BlockDelta { height: b.height, ..Default::default() };
        self.height = b.height;
        let lim = self.limits.clone();
        for r in &b.receipts {
            if r.user != Address::ZERO {
                self.user(r.user).n_requests += 1;
                d.touched.insert(r.user);
            }
        }
        // (user, tx_hash) pairs that already have a ledger record in this block
        let mut ledger_seen: HashSet<(Address, TxHash, u8)> = HashSet::new();
        for e in &b.events {
            if let Event::Ledger { user, tx_hash, delta, .. } = e {
                ledger_seen.insert((*user, *tx_hash, ledger_kind(delta)));
            }
        }
        for e in &b.events {
            match e {
                Event::Fill(f) => self.on_fill(f, &lim, &mut d),
                Event::OrderUpdate { user, order, status, time } => {
                    let rec = OrderRec { order: order.clone(), status: *status, status_ts: *time };
                    self.last_event_time = self.last_event_time.max(*time);
                    self.on_order(*user, rec.clone(), &lim);
                    d.orders_by_user.entry(*user).or_default().push(rec);
                    d.touched.insert(*user);
                }
                Event::Funding { user, asset, usdc, szi, funding_rate, time } => {
                    let rec = FundingRec { time: *time, asset: *asset, usdc: *usdc, szi: *szi, rate: *funding_rate };
                    self.last_event_time = self.last_event_time.max(*time);
                    let u = self.user(*user);
                    u.funding.push_back(rec.clone());
                    let popped = u.funding.len() > lim.user_funding;
                    if popped {
                        u.funding.pop_front();
                    } else {
                        self.total_records += 1;
                    }
                    let fh = self.funding_hist.entry(*asset).or_default();
                    if fh.back().map(|p| p.time) != Some(*time) {
                        fh.push_back(FundingPoint { time: *time, rate: *funding_rate, premium: premium(*asset) });
                        if fh.len() > lim.funding_points_per_coin {
                            fh.pop_front();
                        }
                    }
                    d.funding_by_user.entry(*user).or_default().push(rec);
                    d.touched.insert(*user);
                }
                Event::Ledger { user, time, tx_hash, delta } => {
                    self.last_event_time = self.last_event_time.max(*time);
                    let rec = LedgerRec { time: *time, hash: *tx_hash, delta: delta.clone() };
                    self.on_ledger(*user, rec.clone(), &lim);
                    d.ledger_by_user.entry(*user).or_default().push(rec.clone());
                    d.touched.insert(*user);
                    // Transfers: also show on the receiving side unless the
                    // state machine emitted a record for it itself.
                    let dest = match delta {
                        LedgerDelta::InternalTransfer { destination, .. } => Some(*destination),
                        LedgerDelta::SpotTransfer { destination, .. } => Some(*destination),
                        _ => None,
                    };
                    if let Some(dest) = dest {
                        if dest != *user && !ledger_seen.contains(&(dest, *tx_hash, ledger_kind(delta))) {
                            ledger_seen.insert((dest, *tx_hash, ledger_kind(delta)));
                            self.on_ledger(dest, rec.clone(), &lim);
                            d.ledger_by_user.entry(dest).or_default().push(rec);
                            d.touched.insert(dest);
                        }
                    }
                }
                Event::BookChanged { asset } => {
                    d.book_changed.insert(*asset);
                }
                Event::Adl { .. } | Event::BadDebtCovered { .. } => {}
            }
        }
        self.enforce_global();
        d
    }

    fn on_fill(&mut self, f: &FillEvent, lim: &IndexerLimits, d: &mut BlockDelta) {
        self.last_event_time = self.last_event_time.max(f.time);
        let sz = f.sz.abs();
        let (buyer, seller) = if f.taker_side.is_buy() { (f.taker, f.maker) } else { (f.maker, f.taker) };
        let tr = TradeRec {
            asset: f.asset,
            side: f.taker_side,
            px: f.px,
            sz,
            time: f.time,
            hash: f.tx_hash,
            tid: f.tid,
            buyer,
            seller,
        };
        let tq = self.trades.entry(f.asset).or_default();
        tq.push_back(tr.clone());
        if tq.len() > lim.trades_per_coin {
            tq.pop_front();
        }
        d.trades_by_asset.entry(f.asset).or_default().push(tr);
        let ntl = notional(f.px, sz);
        for iv in ALL_INTERVALS {
            let (t, close_t) = iv.bucket(f.time);
            let q = self.candles.entry((f.asset, iv)).or_default();
            match q.back_mut() {
                Some(c) if c.t == t => {
                    c.c = f.px;
                    c.h = c.h.max(f.px);
                    c.l = c.l.min(f.px);
                    c.v += sz.raw();
                    c.ntl += ntl;
                    c.n += 1;
                }
                Some(c) if c.t > t => {
                    // out-of-order fill (should not happen): fold into the latest candle
                    c.v += sz.raw();
                    c.ntl += ntl;
                    c.n += 1;
                }
                _ => {
                    q.push_back(Candle { t, close_t, o: f.px, c: f.px, h: f.px, l: f.px, v: sz.raw(), ntl, n: 1 });
                    if q.len() > lim.candles_per_series {
                        q.pop_front();
                    }
                }
            }
            d.candles_changed.insert((f.asset, iv));
        }
        *self.daily_exchange.entry(f.time / 86_400_000).or_default() += ntl;
        while self.daily_exchange.len() > 60 {
            self.daily_exchange.pop_first();
        }
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
        for (who, rec) in [(f.taker, taker), (f.maker, maker)] {
            let u = self.user(who);
            u.cum_vlm += ntl;
            u.fills.push_back(rec.clone());
            let popped = u.fills.len() > lim.user_fills;
            if popped {
                u.fills.pop_front();
            } else {
                self.total_records += 1;
            }
            d.fills_by_user.entry(who).or_default().push(rec);
            d.touched.insert(who);
        }
    }

    fn on_order(&mut self, user: Address, rec: OrderRec, lim: &IndexerLimits) {
        let oid = rec.order.oid;
        let cloid = rec.order.cloid;
        let u = self.user(user);
        let mut added = 0usize;
        let mut removed = 0usize;
        match u.orders.get_mut(&oid) {
            Some(existing) => {
                // Keep the original order description (orig size, placement
                // time) but take the latest remaining size and status.
                let orig_sz = existing.order.orig_sz;
                existing.order = rec.order;
                if existing.order.orig_sz.is_zero() {
                    existing.order.orig_sz = orig_sz;
                }
                existing.status = rec.status;
                existing.status_ts = rec.status_ts;
            }
            None => {
                u.orders.insert(oid, rec);
                u.order_seq.push_back(oid);
                added += 1;
                while u.order_seq.len() > lim.user_orders {
                    if let Some(old) = u.order_seq.pop_front() {
                        if let Some(o) = u.orders.remove(&old) {
                            if let Some(c) = o.order.cloid {
                                if u.cloids.get(&c) == Some(&old) {
                                    u.cloids.remove(&c);
                                }
                            }
                        }
                        removed += 1;
                    }
                }
            }
        }
        if let Some(c) = cloid {
            if u.orders.contains_key(&oid) {
                u.cloids.insert(c, oid);
            }
        }
        self.total_records = self.total_records + added - removed;
    }

    fn on_ledger(&mut self, user: Address, rec: LedgerRec, lim: &IndexerLimits) {
        let u = self.user(user);
        u.ledger.push_back(rec);
        let popped = u.ledger.len() > lim.user_ledger;
        if popped {
            u.ledger.pop_front();
        } else {
            self.total_records += 1;
        }
    }

    fn enforce_global(&mut self) {
        while self.total_records > self.limits.max_records || self.users.len() > self.limits.max_users {
            let Some((_, a)) = self.lru.pop_first() else { break };
            if let Some(u) = self.users.remove(&a) {
                self.total_records -= u.records();
            }
        }
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
        self.candles
            .get(&(asset, iv))
            .map(|q| q.iter().filter(|c| c.close_t >= start && c.t <= end).cloned().collect())
            .unwrap_or_default()
    }

    pub fn last_candle(&self, asset: AssetId, iv: Interval) -> Option<Candle> {
        self.candles.get(&(asset, iv)).and_then(|q| q.back().cloned())
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
        self.candles
            .get(&(asset, Interval::H1))
            .map(|q| q.iter().rev().take_while(|c| c.close_t >= from).map(|c| c.v).sum())
            .unwrap_or(0)
    }

    /// Quote notional traded in the last 24 h (1h candles), USD raw.
    pub fn day_ntl_vlm(&self, asset: AssetId, now: u64) -> i128 {
        let from = now.saturating_sub(24 * 3_600_000);
        self.candles
            .get(&(asset, Interval::H1))
            .map(|q| q.iter().rev().take_while(|c| c.close_t >= from).map(|c| c.ntl).sum())
            .unwrap_or(0)
    }

    /// Price 24 h ago: close of the last 1h candle that ended before the
    /// window, else the open of the oldest candle inside it.
    pub fn prev_day_px(&self, asset: AssetId, now: u64) -> Option<Px> {
        let from = now.saturating_sub(24 * 3_600_000);
        let q = self.candles.get(&(asset, Interval::H1))?;
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
