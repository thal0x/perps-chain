//! Straightforward price-time priority book (test fake).

use borsh::{BorshDeserialize, BorshSerialize};
use interfaces::*;
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Debug, Default, BorshSerialize, BorshDeserialize)]
pub struct SimpleBook {
    pub asset: AssetId,
    orders: BTreeMap<Oid, RestingOrder>,
    bids: BTreeMap<Px, VecDeque<Oid>>,
    asks: BTreeMap<Px, VecDeque<Oid>>,
}

impl SimpleBook {
    pub fn new(asset: AssetId) -> Self {
        Self { asset, ..Default::default() }
    }
    pub fn decode(b: &[u8]) -> Result<Self, String> {
        Self::try_from_slice(b).map_err(|e| e.to_string())
    }
    fn best_opposite(&self, side: Side) -> Option<Px> {
        match side {
            Side::Bid => self.asks.keys().next().copied(),
            Side::Ask => self.bids.keys().next_back().copied(),
        }
    }
    fn remove_from_level(&mut self, side: Side, px: Px, oid: Oid) {
        let lv = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        if let Some(q) = lv.get_mut(&px) {
            q.retain(|o| *o != oid);
            if q.is_empty() {
                lv.remove(&px);
            }
        }
    }
}

fn crosses(side: Side, limit: Px, best: Px) -> bool {
    match side {
        Side::Bid => best <= limit,
        Side::Ask => best >= limit,
    }
}

impl OrderBook for SimpleBook {
    fn asset(&self) -> AssetId {
        self.asset
    }
    fn place(&mut self, mut o: RestingOrder, hook: &mut dyn MatchHook) -> PlaceOutcome {
        let mut out = PlaceOutcome {
            filled_sz: Sz::ZERO,
            avg_px: None,
            resting_sz: Sz::ZERO,
            canceled_makers: vec![],
            filled_makers: vec![],
            taker_canceled: false,
            alo_rejected: false,
        };
        if o.tif == Tif::Alo {
            if let Some(b) = self.best_opposite(o.side) {
                if crosses(o.side, o.limit_px, b) {
                    out.alo_rejected = true;
                    out.taker_canceled = true;
                    return out;
                }
            }
        }
        let mut ntl: i128 = 0;
        let mut stopped = false;
        while o.sz.0 > 0 {
            let Some(px) = self.best_opposite(o.side) else { break };
            if !crosses(o.side, o.limit_px, px) {
                break;
            }
            let lv = match o.side {
                Side::Bid => &self.asks,
                Side::Ask => &self.bids,
            };
            let moid = *lv[&px].front().expect("non-empty level");
            let maker = &self.orders[&moid];
            let cand = CandidateFill {
                maker,
                taker_user: o.user,
                taker_oid: o.oid,
                taker_side: o.side,
                taker_reduce_only: o.reduce_only,
                px,
                sz: o.sz.min(maker.sz),
            };
            let max = cand.sz;
            let (d, acc) = hook.on_match(&cand);
            match d {
                MatchDecision::Fill => {
                    assert!(acc.0 > 0 && acc <= max, "hook accepted invalid size");
                    o.sz -= acc;
                    out.filled_sz += acc;
                    ntl += px.0 as i128 * acc.0 as i128;
                    let m = self.orders.get_mut(&moid).expect("maker");
                    m.sz -= acc;
                    if m.sz.0 == 0 {
                        let side = m.side;
                        self.orders.remove(&moid);
                        self.remove_from_level(side, px, moid);
                        out.filled_makers.push(moid);
                    }
                }
                MatchDecision::CancelMaker(why) => {
                    let m = self.orders.remove(&moid).expect("maker");
                    self.remove_from_level(m.side, px, moid);
                    out.canceled_makers.push((m, why));
                }
                MatchDecision::StopTaker => {
                    stopped = true;
                    break;
                }
            }
        }
        if out.filled_sz.0 > 0 {
            out.avg_px = Some(Px((ntl / out.filled_sz.0 as i128) as u64));
        }
        if o.sz.0 > 0 {
            let _ = stopped;
            let rests = matches!(o.tif, Tif::Gtc | Tif::Alo);
            if rests {
                out.resting_sz = o.sz;
                let lv = match o.side {
                    Side::Bid => &mut self.bids,
                    Side::Ask => &mut self.asks,
                };
                lv.entry(o.limit_px).or_default().push_back(o.oid);
                self.orders.insert(o.oid, o);
            } else {
                out.taker_canceled = true;
            }
        }
        out
    }
    fn cancel(&mut self, oid: Oid) -> Option<RestingOrder> {
        let o = self.orders.remove(&oid)?;
        self.remove_from_level(o.side, o.limit_px, oid);
        Some(o)
    }
    fn get(&self, oid: Oid) -> Option<&RestingOrder> {
        self.orders.get(&oid)
    }
    fn get_by_cloid(&self, user: &Address, cloid: &Cloid) -> Option<&RestingOrder> {
        self.orders.values().find(|o| &o.user == user && o.cloid.as_ref() == Some(cloid))
    }
    fn best_bid(&self) -> Option<Px> {
        self.bids.keys().next_back().copied()
    }
    fn best_ask(&self) -> Option<Px> {
        self.asks.keys().next().copied()
    }
    fn levels(&self, side: Side, max_levels: usize) -> Vec<(Px, Sz, u32)> {
        let f = |(px, q): (&Px, &VecDeque<Oid>)| {
            let sz = q.iter().fold(Sz::ZERO, |a, o| a + self.orders[o].sz);
            (*px, sz, q.len() as u32)
        };
        match side {
            Side::Bid => self.bids.iter().rev().take(max_levels).map(f).collect(),
            Side::Ask => self.asks.iter().take(max_levels).map(f).collect(),
        }
    }
    fn user_orders(&self, user: &Address) -> Vec<&RestingOrder> {
        self.orders.values().filter(|o| &o.user == user).collect()
    }
    fn len(&self) -> usize {
        self.orders.len()
    }
    fn encode(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("encode")
    }
}

/// Trigger store fake.
#[derive(Clone, Debug, Default, BorshSerialize, BorshDeserialize)]
pub struct SimpleTriggers {
    orders: BTreeMap<Oid, TriggerOrder>,
}

impl SimpleTriggers {
    pub fn decode(b: &[u8]) -> Result<Self, String> {
        Self::try_from_slice(b).map_err(|e| e.to_string())
    }
}

pub fn trigger_condition(t: &TriggerOrder, mark: Px) -> bool {
    match (t.tpsl, t.side) {
        (TpSl::Tp, Side::Ask) | (TpSl::Sl, Side::Bid) => mark >= t.trigger_px,
        (TpSl::Tp, Side::Bid) | (TpSl::Sl, Side::Ask) => mark <= t.trigger_px,
    }
}

impl TriggerStore for SimpleTriggers {
    fn insert(&mut self, o: TriggerOrder) {
        self.orders.insert(o.oid, o);
    }
    fn remove(&mut self, oid: Oid) -> Option<TriggerOrder> {
        self.orders.remove(&oid)
    }
    fn get(&self, oid: Oid) -> Option<&TriggerOrder> {
        self.orders.get(&oid)
    }
    fn take_triggered(&mut self, mark: Px) -> Vec<TriggerOrder> {
        let oids: Vec<Oid> = self
            .orders
            .values()
            .filter(|t| t.parent_oid.is_none() && trigger_condition(t, mark))
            .map(|t| t.oid)
            .collect();
        oids.into_iter().map(|o| self.orders.remove(&o).expect("present")).collect()
    }
    fn release_children(&mut self, parent: Oid) -> Vec<Oid> {
        let mut v = vec![];
        for t in self.orders.values_mut() {
            if t.parent_oid == Some(parent) {
                t.parent_oid = None;
                v.push(t.oid);
            }
        }
        v
    }
    fn remove_children(&mut self, parent: Oid) -> Vec<TriggerOrder> {
        let oids: Vec<Oid> = self.orders.values().filter(|t| t.parent_oid == Some(parent)).map(|t| t.oid).collect();
        oids.into_iter().map(|o| self.orders.remove(&o).expect("present")).collect()
    }
    fn user_orders(&self, user: &Address) -> Vec<&TriggerOrder> {
        self.orders.values().filter(|o| &o.user == user).collect()
    }
    fn len(&self) -> usize {
        self.orders.len()
    }
    fn encode(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("encode")
    }
}
