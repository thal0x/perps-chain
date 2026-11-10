//! Parked trigger (TP/SL) orders of one market.
//!
//! Semantics (from `interfaces::TriggerStore`):
//! * Direction: TP sell / SL buy fire when `mark >= trigger_px` ("up");
//!   TP buy / SL sell fire when `mark <= trigger_px` ("down").
//! * A child with `parent_oid = Some(p)` (normalTpsl grouping) is parked but
//!   not eligible until its parent fills: `release_children(p)` clears the
//!   link and makes the children eligible; `remove_children(p)` (parent
//!   canceled/rejected) removes them.
//! * `take_triggered(mark)` removes and returns all eligible orders whose
//!   condition holds, ordered by oid.
//!
//! Indexes: `up`/`down` are `BTreeSet<(trigger_px, oid)>` holding only
//! eligible orders, so `take_triggered` is O(k log n) for k triggered orders.

use borsh::{BorshDeserialize, BorshSerialize};
use interfaces::*;
use std::collections::{BTreeMap, BTreeSet};

const ENCODING_VERSION: u8 = 1;

/// True if the order fires when the mark rises to/above `trigger_px`.
pub fn fires_on_rise(o: &TriggerOrder) -> bool {
    matches!((o.tpsl, o.side), (TpSl::Tp, Side::Ask) | (TpSl::Sl, Side::Bid))
}

/// Does `o`'s trigger condition hold at `mark` (ignoring parent linkage)?
pub fn condition_holds(o: &TriggerOrder, mark: Px) -> bool {
    if fires_on_rise(o) {
        mark >= o.trigger_px
    } else {
        mark <= o.trigger_px
    }
}

#[derive(BorshSerialize, BorshDeserialize)]
struct TriggerImage {
    version: u8,
    asset: AssetId,
    /// Ascending oid.
    orders: Vec<TriggerOrder>,
}

/// Errors from [`TriggerBook::decode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerDecodeError {
    Borsh(String),
    BadVersion(u8),
    NonCanonical(&'static str),
}

impl std::fmt::Display for TriggerDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for TriggerDecodeError {}

#[derive(Clone, Debug, Default)]
pub struct TriggerBook {
    asset: AssetId,
    orders: BTreeMap<Oid, TriggerOrder>,
    up: BTreeSet<(Px, Oid)>,
    down: BTreeSet<(Px, Oid)>,
    children: BTreeMap<Oid, BTreeSet<Oid>>,
    by_user: BTreeMap<Address, BTreeSet<Oid>>,
}

impl TriggerBook {
    pub fn new(asset: AssetId) -> Self {
        TriggerBook { asset, ..Default::default() }
    }

    pub fn asset(&self) -> AssetId {
        self.asset
    }

    fn index_eligible(&mut self, o: &TriggerOrder) {
        let k = (o.trigger_px, o.oid);
        if fires_on_rise(o) {
            self.up.insert(k);
        } else {
            self.down.insert(k);
        }
    }

    fn unindex_eligible(&mut self, o: &TriggerOrder) {
        let k = (o.trigger_px, o.oid);
        if fires_on_rise(o) {
            self.up.remove(&k);
        } else {
            self.down.remove(&k);
        }
    }

    /// All orders, ascending oid.
    pub fn iter(&self) -> impl Iterator<Item = &TriggerOrder> {
        self.orders.values()
    }

    /// Oids of children still waiting on `parent`, ascending.
    pub fn children_of(&self, parent: Oid) -> Vec<Oid> {
        self.children.get(&parent).map(|s| s.iter().copied().collect()).unwrap_or_default()
    }

    /// Update the size of a parked order (e.g. resizing a position TP/SL).
    /// Returns false if the oid is unknown.
    pub fn update_sz(&mut self, oid: Oid, sz: Sz) -> bool {
        match self.orders.get_mut(&oid) {
            Some(o) => {
                o.sz = sz;
                true
            }
            None => false,
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, TriggerDecodeError> {
        let img = TriggerImage::try_from_slice(bytes).map_err(|e| TriggerDecodeError::Borsh(e.to_string()))?;
        if img.version != ENCODING_VERSION {
            return Err(TriggerDecodeError::BadVersion(img.version));
        }
        let mut t = TriggerBook::new(img.asset);
        let mut prev: Option<Oid> = None;
        for o in img.orders {
            if prev.is_some_and(|p| p >= o.oid) {
                return Err(TriggerDecodeError::NonCanonical("oids not strictly ascending"));
            }
            if o.asset != img.asset {
                return Err(TriggerDecodeError::NonCanonical("order of another asset"));
            }
            prev = Some(o.oid);
            t.insert(o);
        }
        Ok(t)
    }

    /// Internal consistency check (tests). Panics on violation.
    pub fn check_invariants(&self) {
        let mut up = BTreeSet::new();
        let mut down = BTreeSet::new();
        let mut children: BTreeMap<Oid, BTreeSet<Oid>> = BTreeMap::new();
        let mut by_user: BTreeMap<Address, BTreeSet<Oid>> = BTreeMap::new();
        for (oid, o) in &self.orders {
            assert_eq!(*oid, o.oid);
            assert_eq!(o.asset, self.asset);
            by_user.entry(o.user).or_default().insert(o.oid);
            match o.parent_oid {
                Some(p) => {
                    children.entry(p).or_default().insert(o.oid);
                }
                None => {
                    if fires_on_rise(o) {
                        up.insert((o.trigger_px, o.oid));
                    } else {
                        down.insert((o.trigger_px, o.oid));
                    }
                }
            }
        }
        assert_eq!(up, self.up);
        assert_eq!(down, self.down);
        assert_eq!(children, self.children);
        assert_eq!(by_user, self.by_user);
    }
}

impl TriggerStore for TriggerBook {
    /// Panics on duplicate oid or wrong asset (caller bug).
    fn insert(&mut self, o: TriggerOrder) {
        assert_eq!(o.asset, self.asset, "trigger order for another asset");
        assert!(!self.orders.contains_key(&o.oid), "duplicate trigger oid {}", o.oid);
        match o.parent_oid {
            Some(p) => {
                self.children.entry(p).or_default().insert(o.oid);
            }
            None => self.index_eligible(&o),
        }
        self.by_user.entry(o.user).or_default().insert(o.oid);
        self.orders.insert(o.oid, o);
    }

    fn remove(&mut self, oid: Oid) -> Option<TriggerOrder> {
        let o = self.orders.remove(&oid)?;
        match o.parent_oid {
            Some(p) => {
                if let Some(s) = self.children.get_mut(&p) {
                    s.remove(&oid);
                    if s.is_empty() {
                        self.children.remove(&p);
                    }
                }
            }
            None => self.unindex_eligible(&o),
        }
        if let Some(s) = self.by_user.get_mut(&o.user) {
            s.remove(&oid);
            if s.is_empty() {
                self.by_user.remove(&o.user);
            }
        }
        Some(o)
    }

    fn get(&self, oid: Oid) -> Option<&TriggerOrder> {
        self.orders.get(&oid)
    }

    fn take_triggered(&mut self, mark: Px) -> Vec<TriggerOrder> {
        let mut oids: Vec<Oid> = self.up.range(..=(mark, Oid::MAX)).map(|(_, o)| *o).collect();
        oids.extend(self.down.range((mark, 0)..).map(|(_, o)| *o));
        oids.sort_unstable();
        oids.into_iter().map(|oid| self.remove(oid).expect("indexed trigger exists")).collect()
    }

    fn release_children(&mut self, parent: Oid) -> Vec<Oid> {
        let Some(set) = self.children.remove(&parent) else { return Vec::new() };
        let oids: Vec<Oid> = set.into_iter().collect();
        for oid in &oids {
            let o = self.orders.get_mut(oid).expect("child exists");
            o.parent_oid = None;
            let o = o.clone();
            self.index_eligible(&o);
        }
        oids
    }

    fn remove_children(&mut self, parent: Oid) -> Vec<TriggerOrder> {
        let Some(set) = self.children.get(&parent) else { return Vec::new() };
        let oids: Vec<Oid> = set.iter().copied().collect();
        oids.into_iter().map(|oid| self.remove(oid).expect("child exists")).collect()
    }

    fn user_orders(&self, user: &Address) -> Vec<&TriggerOrder> {
        match self.by_user.get(user) {
            Some(s) => s.iter().map(|oid| &self.orders[oid]).collect(),
            None => Vec::new(),
        }
    }

    fn len(&self) -> usize {
        self.orders.len()
    }

    fn encode(&self) -> Vec<u8> {
        let img = TriggerImage { version: ENCODING_VERSION, asset: self.asset, orders: self.orders.values().cloned().collect() };
        borsh::to_vec(&img).expect("borsh to vec")
    }
}
