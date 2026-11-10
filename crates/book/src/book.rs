//! Limit order book with strict price-time priority.
//!
//! Layout:
//! * `slots`: arena (slab) of order slots; each live slot holds the
//!   `RestingOrder` plus intrusive `prev`/`next` links of its price level's
//!   FIFO queue. Freed slots are recycled through a free list.
//! * `bids` / `asks`: `BTreeMap<Px, Level>`; a `Level` stores head/tail slot
//!   indices, aggregated size and order count. Best bid = last key, best ask =
//!   first key.
//! * `by_oid`, `by_cloid`, `by_user`: point-lookup indexes (fixed-hasher hash
//!   maps). `by_user` maps to a `BTreeSet<Oid>` so per-user listings come out
//!   ordered by oid.
//!
//! Complexities (n orders, L levels): place without match O(log L + log n_u);
//! cancel O(1) unlink + O(log L) level lookup; each fill O(log L).
//!
//! The arena layout, free list and hash maps are *not* part of the logical
//! state: [`Book::encode`] emits only the asset and, per side, the resting
//! orders in priority order (best price first, FIFO within a price). Two books
//! that would behave identically for every future operation encode to the
//! same bytes.

use crate::fxhash::FxMap;
use borsh::{BorshDeserialize, BorshSerialize};
use interfaces::*;
use std::collections::{BTreeMap, BTreeSet};

const NIL: u32 = u32::MAX;
const ENCODING_VERSION: u8 = 1;

#[derive(Clone, Debug)]
struct Slot {
    order: Option<RestingOrder>,
    prev: u32,
    next: u32,
}

#[derive(Clone, Copy, Debug)]
struct Level {
    head: u32,
    tail: u32,
    total: i64,
    count: u32,
}

/// Errors from [`Book::decode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookDecodeError {
    Borsh(String),
    BadVersion(u8),
    /// Bytes are a valid encoding but not the canonical encoding of a valid
    /// book (wrong side/asset, unsorted, crossed, duplicate oid, zero size,
    /// trailing bytes...).
    NonCanonical(&'static str),
}

impl std::fmt::Display for BookDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for BookDecodeError {}

#[derive(BorshSerialize, BorshDeserialize)]
struct BookImage {
    version: u8,
    asset: AssetId,
    /// Best (highest) price first, FIFO within a level.
    bids: Vec<RestingOrder>,
    /// Best (lowest) price first, FIFO within a level.
    asks: Vec<RestingOrder>,
}

/// Single-market limit order book. See module docs.
#[derive(Clone, Debug)]
pub struct Book {
    asset: AssetId,
    slots: Vec<Slot>,
    free: Vec<u32>,
    bids: BTreeMap<Px, Level>,
    asks: BTreeMap<Px, Level>,
    by_oid: FxMap<Oid, u32>,
    by_cloid: FxMap<(Address, Cloid), Oid>,
    by_user: FxMap<Address, BTreeSet<Oid>>,
}

/// Exact running sum of `px * sz` over the fills of one taker order.
/// `avg_px = floor(sum / filled)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NotionalAcc {
    /// Σ px.raw * sz.raw (1e-16 USD units), exact.
    pub px_sz_sum: u128,
    pub filled: i64,
}

impl NotionalAcc {
    pub fn add(&mut self, px: Px, sz: Sz) {
        assert!(sz.0 > 0);
        let p = (px.0 as u128).checked_mul(sz.0 as u128).expect("notional overflow");
        self.px_sz_sum = self.px_sz_sum.checked_add(p).expect("notional overflow");
        self.filled = self.filled.checked_add(sz.0).expect("filled overflow");
    }
    /// Volume-weighted average price, rounded down. None if nothing filled.
    pub fn avg_px(&self) -> Option<Px> {
        if self.filled == 0 {
            return None;
        }
        let q = self.px_sz_sum / self.filled as u128;
        Some(Px(u64::try_from(q).expect("avg px overflow")))
    }
}

#[inline]
fn crosses(taker_side: Side, taker_px: Px, maker_px: Px) -> bool {
    match taker_side {
        Side::Bid => maker_px <= taker_px,
        Side::Ask => maker_px >= taker_px,
    }
}

impl Book {
    pub fn new(asset: AssetId) -> Self {
        Self::with_capacity(asset, 0)
    }

    pub fn with_capacity(asset: AssetId, cap: usize) -> Self {
        let mut by_oid = FxMap::default();
        by_oid.reserve(cap);
        Book {
            asset,
            slots: Vec::with_capacity(cap),
            free: Vec::new(),
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            by_oid,
            by_cloid: FxMap::default(),
            by_user: FxMap::default(),
        }
    }

    #[inline]
    fn side_map(&self, side: Side) -> &BTreeMap<Px, Level> {
        match side {
            Side::Bid => &self.bids,
            Side::Ask => &self.asks,
        }
    }

    #[inline]
    fn side_map_mut(&mut self, side: Side) -> &mut BTreeMap<Px, Level> {
        match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        }
    }

    /// Best level of `side` as (px, head slot).
    #[inline]
    fn best_level(&self, side: Side) -> Option<(Px, u32)> {
        match side {
            Side::Bid => self.bids.last_key_value().map(|(p, l)| (*p, l.head)),
            Side::Ask => self.asks.first_key_value().map(|(p, l)| (*p, l.head)),
        }
    }

    #[inline]
    fn order_at(&self, idx: u32) -> &RestingOrder {
        self.slots[idx as usize].order.as_ref().expect("live slot")
    }

    fn alloc(&mut self, order: RestingOrder) -> u32 {
        let slot = Slot { order: Some(order), prev: NIL, next: NIL };
        match self.free.pop() {
            Some(i) => {
                self.slots[i as usize] = slot;
                i
            }
            None => {
                let i = u32::try_from(self.slots.len()).expect("too many orders");
                assert!(i != NIL, "too many orders");
                self.slots.push(slot);
                i
            }
        }
    }

    /// Append `order` at the tail of its price level (time priority) and index it.
    fn rest(&mut self, order: RestingOrder) {
        debug_assert!(order.sz.0 > 0);
        let oid = order.oid;
        let user = order.user;
        let cloid = order.cloid;
        let side = order.side;
        let px = order.limit_px;
        let sz = order.sz.0;
        let idx = self.alloc(order);
        let level = self
            .side_map_mut(side)
            .entry(px)
            .or_insert(Level { head: NIL, tail: NIL, total: 0, count: 0 });
        let old_tail = level.tail;
        level.tail = idx;
        if old_tail == NIL {
            level.head = idx;
        }
        level.total = level.total.checked_add(sz).expect("level size overflow");
        level.count += 1;
        if old_tail != NIL {
            self.slots[old_tail as usize].next = idx;
            self.slots[idx as usize].prev = old_tail;
        }
        self.by_oid.insert(oid, idx);
        if let Some(c) = cloid {
            self.by_cloid.insert((user, c), oid);
        }
        self.by_user.entry(user).or_default().insert(oid);
    }

    /// Unlink and free slot `idx`, removing it from every index.
    fn remove_slot(&mut self, idx: u32) -> RestingOrder {
        let (prev, next) = {
            let s = &self.slots[idx as usize];
            (s.prev, s.next)
        };
        let order = self.slots[idx as usize].order.take().expect("live slot");
        if prev != NIL {
            self.slots[prev as usize].next = next;
        }
        if next != NIL {
            self.slots[next as usize].prev = prev;
        }
        let map = self.side_map_mut(order.side);
        let level = map.get_mut(&order.limit_px).expect("level of live order");
        if level.head == idx {
            level.head = next;
        }
        if level.tail == idx {
            level.tail = prev;
        }
        level.total -= order.sz.0;
        level.count -= 1;
        if level.count == 0 {
            debug_assert_eq!(level.total, 0);
            map.remove(&order.limit_px);
        }
        self.free.push(idx);
        self.by_oid.remove(&order.oid);
        if let Some(c) = order.cloid {
            let key = (order.user, c);
            if self.by_cloid.get(&key) == Some(&order.oid) {
                self.by_cloid.remove(&key);
            }
        }
        if let Some(set) = self.by_user.get_mut(&order.user) {
            set.remove(&order.oid);
            if set.is_empty() {
                self.by_user.remove(&order.user);
            }
        }
        order
    }

    /// Reduce a resting order's size by `by` (> 0, < its size) in place,
    /// keeping its time priority.
    fn reduce_in_place(&mut self, idx: u32, by: i64) {
        let o = self.slots[idx as usize].order.as_mut().expect("live slot");
        o.sz.0 -= by;
        debug_assert!(o.sz.0 > 0);
        let (side, px) = (o.side, o.limit_px);
        let level = self.side_map_mut(side).get_mut(&px).expect("level");
        level.total -= by;
    }

    /// Would an order of `side` at `px` cross the opposite side right now?
    pub fn would_cross(&self, side: Side, px: Px) -> bool {
        match self.best_level(side.opposite()) {
            Some((best, _)) => crosses(side, px, best),
            None => false,
        }
    }

    /// Reduce a resting order's size without losing priority (HL-style
    /// size decrease). `new_sz` must be in (0, current sz]. Returns false if
    /// the oid is unknown.
    pub fn reduce_size(&mut self, oid: Oid, new_sz: Sz) -> bool {
        let Some(&idx) = self.by_oid.get(&oid) else { return false };
        let cur = self.order_at(idx).sz;
        assert!(new_sz.0 > 0 && new_sz <= cur, "reduce_size: new size must be in (0, current]");
        if new_sz < cur {
            self.reduce_in_place(idx, cur.0 - new_sz.0);
        }
        true
    }

    /// Resting orders of one side in priority order (best price first, FIFO).
    pub fn orders_in_priority(&self, side: Side) -> Vec<&RestingOrder> {
        let mut out = Vec::new();
        let mut push_level = |l: &Level| {
            let mut i = l.head;
            while i != NIL {
                out.push(self.order_at(i));
                i = self.slots[i as usize].next;
            }
        };
        match side {
            Side::Bid => self.bids.values().rev().for_each(&mut push_level),
            Side::Ask => self.asks.values().for_each(&mut push_level),
        }
        out
    }

    /// Decode a canonical encoding produced by [`Book::encode`]. Rejects any
    /// non-canonical input so that `decode(b).encode() == b` always holds.
    pub fn decode(bytes: &[u8]) -> Result<Self, BookDecodeError> {
        let img = BookImage::try_from_slice(bytes).map_err(|e| BookDecodeError::Borsh(e.to_string()))?;
        if img.version != ENCODING_VERSION {
            return Err(BookDecodeError::BadVersion(img.version));
        }
        let mut b = Book::with_capacity(img.asset, img.bids.len() + img.asks.len());
        for (side, list) in [(Side::Bid, &img.bids), (Side::Ask, &img.asks)] {
            let mut prev_px: Option<Px> = None;
            for o in list {
                if o.side != side {
                    return Err(BookDecodeError::NonCanonical("order on wrong side"));
                }
                if o.asset != img.asset {
                    return Err(BookDecodeError::NonCanonical("order of another asset"));
                }
                if o.sz.0 <= 0 {
                    return Err(BookDecodeError::NonCanonical("bad order size"));
                }
                if o.tif == Tif::Ioc {
                    return Err(BookDecodeError::NonCanonical("resting IOC order"));
                }
                if let Some(p) = prev_px {
                    let ok = match side {
                        Side::Bid => o.limit_px <= p,
                        Side::Ask => o.limit_px >= p,
                    };
                    if !ok {
                        return Err(BookDecodeError::NonCanonical("orders not in priority order"));
                    }
                }
                prev_px = Some(o.limit_px);
                if b.by_oid.contains_key(&o.oid) {
                    return Err(BookDecodeError::NonCanonical("duplicate oid"));
                }
                b.rest(o.clone());
            }
        }
        if let (Some(bb), Some(ba)) = (b.best_bid(), b.best_ask()) {
            if bb >= ba {
                return Err(BookDecodeError::NonCanonical("crossed book"));
            }
        }
        Ok(b)
    }

    /// Full internal consistency check (tests / debug). Panics with a message
    /// on the first violation.
    pub fn check_invariants(&self) {
        let mut seen = 0usize;
        for side in [Side::Bid, Side::Ask] {
            for (px, l) in self.side_map(side) {
                assert!(l.count > 0, "empty level kept");
                let mut i = l.head;
                let mut prev = NIL;
                let mut n = 0u32;
                let mut tot = 0i64;
                while i != NIL {
                    let s = &self.slots[i as usize];
                    let o = s.order.as_ref().expect("linked slot is live");
                    assert_eq!(s.prev, prev, "broken prev link");
                    assert_eq!(o.limit_px, *px);
                    assert_eq!(o.side, side);
                    assert_eq!(o.asset, self.asset);
                    assert!(o.sz.0 > 0);
                    assert_eq!(self.by_oid.get(&o.oid), Some(&i));
                    assert!(self.by_user.get(&o.user).is_some_and(|s| s.contains(&o.oid)));
                    if let Some(c) = o.cloid {
                        assert!(self.by_cloid.contains_key(&(o.user, c)));
                    }
                    n += 1;
                    tot += o.sz.0;
                    prev = i;
                    i = s.next;
                }
                assert_eq!(l.tail, prev, "bad tail");
                assert_eq!(n, l.count, "bad count");
                assert_eq!(tot, l.total, "bad total");
                seen += n as usize;
            }
        }
        assert_eq!(seen, self.by_oid.len(), "oid index size");
        assert_eq!(seen + self.free.len(), self.slots.len(), "slab accounting");
        assert_eq!(self.by_user.values().map(|s| s.len()).sum::<usize>(), seen);
        if let (Some(bb), Some(ba)) = (self.best_bid(), self.best_ask()) {
            assert!(bb < ba, "crossed book");
        }
    }
}

impl OrderBook for Book {
    fn asset(&self) -> AssetId {
        self.asset
    }

    /// Matching semantics:
    /// * Candidates are offered in strict price-time priority at the maker's
    ///   price, with `sz = min(taker remaining, maker remaining)`.
    /// * `Fill` with accepted size `a` (0 < a <= sz, else panic): both sides
    ///   are reduced by `a`; matching continues (a partially accepted maker
    ///   that still has size is offered again with the new remaining sizes;
    ///   every `Fill` makes progress so this terminates).
    /// * `CancelMaker(r)`: maker removed, reported in `canceled_makers`,
    ///   matching continues.
    /// * `StopTaker`: matching stops and the taker remainder is **canceled
    ///   regardless of TIF** (resting it would cross the book; see
    ///   INTERFACE_REQUESTS.md).
    /// * After matching (no stop): GTC rests the remainder, IOC cancels it.
    /// * ALO: if it would cross, rejected without any hook call
    ///   (`alo_rejected`, `taker_canceled`); otherwise rests.
    fn place(&mut self, order: RestingOrder, hook: &mut dyn MatchHook) -> PlaceOutcome {
        assert!(order.sz.0 > 0, "order size must be positive");
        assert_eq!(order.asset, self.asset, "order for another asset");
        assert!(!self.by_oid.contains_key(&order.oid), "duplicate oid {}", order.oid);

        let mut out = PlaceOutcome {
            filled_sz: Sz::ZERO,
            avg_px: None,
            resting_sz: Sz::ZERO,
            canceled_makers: Vec::new(),
            filled_makers: Vec::new(),
            taker_canceled: false,
            alo_rejected: false,
        };

        if order.tif == Tif::Alo {
            if self.would_cross(order.side, order.limit_px) {
                out.alo_rejected = true;
                out.taker_canceled = true;
            } else {
                out.resting_sz = order.sz;
                self.rest(order);
            }
            return out;
        }

        let mut rem = order.sz.0;
        let mut acc = NotionalAcc::default();
        let mut stopped = false;
        let maker_side = order.side.opposite();
        while rem > 0 {
            let Some((lpx, head)) = self.best_level(maker_side) else { break };
            if !crosses(order.side, order.limit_px, lpx) {
                break;
            }
            let maker = self.order_at(head);
            let cand_sz = rem.min(maker.sz.0);
            let c = CandidateFill {
                maker,
                taker_user: order.user,
                taker_oid: order.oid,
                taker_side: order.side,
                taker_reduce_only: order.reduce_only,
                px: lpx,
                sz: Sz(cand_sz),
            };
            let (decision, accepted) = hook.on_match(&c);
            match decision {
                MatchDecision::Fill => {
                    let a = accepted.0;
                    assert!(a > 0 && a <= cand_sz, "hook accepted size {a} outside (0, {cand_sz}]");
                    acc.add(lpx, accepted);
                    rem -= a;
                    if a == self.order_at(head).sz.0 {
                        let m = self.remove_slot(head);
                        out.filled_makers.push(m.oid);
                    } else {
                        self.reduce_in_place(head, a);
                    }
                }
                MatchDecision::CancelMaker(reason) => {
                    let m = self.remove_slot(head);
                    out.canceled_makers.push((m, reason));
                }
                MatchDecision::StopTaker => {
                    stopped = true;
                    break;
                }
            }
        }

        out.filled_sz = Sz(acc.filled);
        out.avg_px = acc.avg_px();
        if rem > 0 {
            if stopped || order.tif == Tif::Ioc {
                out.taker_canceled = true;
            } else {
                let mut o = order;
                o.sz = Sz(rem);
                out.resting_sz = o.sz;
                self.rest(o);
            }
        }
        out
    }

    fn cancel(&mut self, oid: Oid) -> Option<RestingOrder> {
        let idx = *self.by_oid.get(&oid)?;
        Some(self.remove_slot(idx))
    }

    fn get(&self, oid: Oid) -> Option<&RestingOrder> {
        self.by_oid.get(&oid).map(|&i| self.order_at(i))
    }

    fn get_by_cloid(&self, user: &Address, cloid: &Cloid) -> Option<&RestingOrder> {
        let oid = self.by_cloid.get(&(*user, *cloid))?;
        self.get(*oid)
    }

    fn best_bid(&self) -> Option<Px> {
        self.bids.last_key_value().map(|(p, _)| *p)
    }

    fn best_ask(&self) -> Option<Px> {
        self.asks.first_key_value().map(|(p, _)| *p)
    }

    fn levels(&self, side: Side, max_levels: usize) -> Vec<(Px, Sz, u32)> {
        let f = |(p, l): (&Px, &Level)| (*p, Sz(l.total), l.count);
        match side {
            Side::Bid => self.bids.iter().rev().take(max_levels).map(f).collect(),
            Side::Ask => self.asks.iter().take(max_levels).map(f).collect(),
        }
    }

    fn user_orders(&self, user: &Address) -> Vec<&RestingOrder> {
        match self.by_user.get(user) {
            Some(set) => set.iter().map(|oid| self.order_at(self.by_oid[oid])).collect(),
            None => Vec::new(),
        }
    }

    fn len(&self) -> usize {
        self.by_oid.len()
    }

    fn encode(&self) -> Vec<u8> {
        let img = BookImage {
            version: ENCODING_VERSION,
            asset: self.asset,
            bids: self.orders_in_priority(Side::Bid).into_iter().cloned().collect(),
            asks: self.orders_in_priority(Side::Ask).into_iter().cloned().collect(),
        };
        borsh::to_vec(&img).expect("borsh to vec")
    }
}
