#![allow(dead_code)]
use interfaces::*;

pub fn addr(i: u8) -> Address {
    let mut a = [0u8; 20];
    a[19] = i;
    Address(a)
}

pub fn px(v: u64) -> Px {
    Px(v * 100_000_000)
}

pub fn sz(v: i64) -> Sz {
    Sz(v * 100_000_000)
}

pub fn order(oid: Oid, user: u8, side: Side, p: u64, s: i64, tif: Tif) -> RestingOrder {
    RestingOrder {
        oid,
        user: addr(user),
        asset: 0,
        side,
        limit_px: px(p),
        sz: sz(s),
        orig_sz: sz(s),
        reduce_only: false,
        cloid: None,
        timestamp: oid,
        tif,
    }
}

/// Hook that fills everything and records the candidates it saw.
#[derive(Default)]
pub struct FillAll {
    pub seen: Vec<(Oid, Px, Sz)>,
}

impl MatchHook for FillAll {
    fn on_match(&mut self, c: &CandidateFill<'_>) -> (MatchDecision, Sz) {
        self.seen.push((c.maker.oid, c.px, c.sz));
        (MatchDecision::Fill, c.sz)
    }
}

/// Hook driven by a closure.
pub struct FnHook<F: FnMut(&CandidateFill<'_>) -> (MatchDecision, Sz)>(pub F);

impl<F: FnMut(&CandidateFill<'_>) -> (MatchDecision, Sz)> MatchHook for FnHook<F> {
    fn on_match(&mut self, c: &CandidateFill<'_>) -> (MatchDecision, Sz) {
        (self.0)(c)
    }
}
