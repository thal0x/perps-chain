//! Property test: `Book` vs a naive Vec-based reference book over random op
//! sequences with a scripted (pseudo-random but pure) match hook.

mod common;
use book::*;
use common::*;
use interfaces::*;
use proptest::prelude::*;

// ------------------------------------------------------------ reference ---

#[derive(Default)]
struct RefBook {
    /// (arrival seq, order)
    orders: Vec<(u64, RestingOrder)>,
    seq: u64,
}

fn crosses(taker: Side, tpx: Px, mpx: Px) -> bool {
    match taker {
        Side::Bid => mpx <= tpx,
        Side::Ask => mpx >= tpx,
    }
}

impl RefBook {
    /// Index of the best maker for a taker of `side`, by (price, seq).
    fn best_maker(&self, side: Side) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, (seq, o)) in self.orders.iter().enumerate() {
            if o.side != side.opposite() {
                continue;
            }
            best = match best {
                None => Some(i),
                Some(j) => {
                    let (bseq, b) = &self.orders[j];
                    let better = match side {
                        Side::Bid => (o.limit_px, *seq) < (b.limit_px, *bseq),
                        Side::Ask => (std::cmp::Reverse(o.limit_px), *seq) < (std::cmp::Reverse(b.limit_px), *bseq),
                    };
                    if better {
                        Some(i)
                    } else {
                        Some(j)
                    }
                }
            };
        }
        best
    }

    fn rest(&mut self, o: RestingOrder) {
        self.seq += 1;
        self.orders.push((self.seq, o));
    }

    fn place(&mut self, order: RestingOrder, hook: &mut dyn MatchHook) -> PlaceOutcome {
        let mut out = PlaceOutcome {
            filled_sz: Sz::ZERO,
            avg_px: None,
            resting_sz: Sz::ZERO,
            canceled_makers: vec![],
            filled_makers: vec![],
            taker_canceled: false,
            alo_rejected: false,
        };
        if order.tif == Tif::Alo {
            if let Some(i) = self.best_maker(order.side) {
                if crosses(order.side, order.limit_px, self.orders[i].1.limit_px) {
                    out.alo_rejected = true;
                    out.taker_canceled = true;
                    return out;
                }
            }
            out.resting_sz = order.sz;
            self.rest(order);
            return out;
        }
        let mut rem = order.sz.0;
        let mut num: u128 = 0;
        let mut filled: i64 = 0;
        let mut stopped = false;
        while rem > 0 {
            let Some(i) = self.best_maker(order.side) else { break };
            let mpx = self.orders[i].1.limit_px;
            if !crosses(order.side, order.limit_px, mpx) {
                break;
            }
            let csz = rem.min(self.orders[i].1.sz.0);
            let (d, a) = hook.on_match(&CandidateFill {
                maker: &self.orders[i].1,
                taker_user: order.user,
                taker_oid: order.oid,
                taker_side: order.side,
                taker_reduce_only: order.reduce_only,
                px: mpx,
                sz: Sz(csz),
            });
            match d {
                MatchDecision::Fill => {
                    rem -= a.0;
                    filled += a.0;
                    num += mpx.0 as u128 * a.0 as u128;
                    self.orders[i].1.sz.0 -= a.0;
                    if self.orders[i].1.sz.0 == 0 {
                        let (_, m) = self.orders.remove(i);
                        out.filled_makers.push(m.oid);
                    }
                }
                MatchDecision::CancelMaker(r) => {
                    let (_, m) = self.orders.remove(i);
                    out.canceled_makers.push((m, r));
                }
                MatchDecision::StopTaker => {
                    stopped = true;
                    break;
                }
            }
        }
        out.filled_sz = Sz(filled);
        if filled > 0 {
            out.avg_px = Some(Px((num / filled as u128) as u64));
        }
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
        let i = self.orders.iter().position(|(_, o)| o.oid == oid)?;
        Some(self.orders.remove(i).1)
    }

    fn sorted_side(&self, side: Side) -> Vec<RestingOrder> {
        let mut v: Vec<_> = self.orders.iter().filter(|(_, o)| o.side == side).cloned().collect();
        match side {
            Side::Bid => v.sort_by_key(|(s, o)| (std::cmp::Reverse(o.limit_px), *s)),
            Side::Ask => v.sort_by_key(|(s, o)| (o.limit_px, *s)),
        }
        v.into_iter().map(|(_, o)| o).collect()
    }

    fn levels(&self, side: Side) -> Vec<(Px, Sz, u32)> {
        let mut out: Vec<(Px, Sz, u32)> = vec![];
        for o in self.sorted_side(side) {
            match out.last_mut() {
                Some(l) if l.0 == o.limit_px => {
                    l.1 = l.1 + o.sz;
                    l.2 += 1;
                }
                _ => out.push((o.limit_px, o.sz, 1)),
            }
        }
        out
    }

    fn encode(&self, asset: AssetId) -> Vec<u8> {
        #[derive(borsh::BorshSerialize)]
        struct Img {
            version: u8,
            asset: AssetId,
            bids: Vec<RestingOrder>,
            asks: Vec<RestingOrder>,
        }
        borsh::to_vec(&Img { version: 1, asset, bids: self.sorted_side(Side::Bid), asks: self.sorted_side(Side::Ask) })
            .unwrap()
    }
}

// --------------------------------------------------------------- hook -----

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Pure scripted hook: decision depends only on the candidate and a call
/// counter, so both books get identical decisions iff they offer identical
/// candidate sequences. Logs every candidate.
struct ScriptHook {
    salt: u64,
    /// Percent weights: fill full, fill partial, cancel maker, stop.
    mode: u8,
    n: u64,
    log: Vec<(Oid, Oid, Px, Sz, Sz)>,
}

impl MatchHook for ScriptHook {
    fn on_match(&mut self, c: &CandidateFill<'_>) -> (MatchDecision, Sz) {
        self.n += 1;
        let r = mix(self.salt ^ c.maker.oid.wrapping_mul(31) ^ c.taker_oid.wrapping_mul(1009) ^ (self.n << 40));
        let roll = r % 100;
        let (d, a) = match self.mode {
            0 => (MatchDecision::Fill, c.sz),
            _ => {
                if roll < 60 {
                    (MatchDecision::Fill, c.sz)
                } else if roll < 80 {
                    let a = 1 + (r >> 8) as i64 % c.sz.0;
                    (MatchDecision::Fill, Sz(a))
                } else if roll < 92 {
                    (MatchDecision::CancelMaker(OrderLifecycle::MarginCanceled), Sz::ZERO)
                } else {
                    (MatchDecision::StopTaker, Sz::ZERO)
                }
            }
        };
        self.log.push((c.maker.oid, c.taker_oid, c.px, c.sz, a));
        (d, a)
    }
}

// ---------------------------------------------------------------- ops -----

#[derive(Clone, Debug)]
enum Op {
    Place { user: u8, side: bool, px: u64, sz: i64, tif: u8, cloid: bool, ro: bool },
    Cancel { pick: usize },
    Reduce { pick: usize, frac: u8 },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0u8..5, any::<bool>(), 95u64..106, 1i64..600, 0u8..3, any::<bool>(), any::<bool>())
            .prop_map(|(user, side, px, sz, tif, cloid, ro)| Op::Place { user, side, px, sz, tif, cloid, ro }),
        3 => any::<usize>().prop_map(|pick| Op::Cancel { pick }),
        1 => (any::<usize>(), 1u8..100).prop_map(|(pick, frac)| Op::Reduce { pick, frac }),
    ]
}

fn run(ops: Vec<Op>, salt: u64, mode: u8) {
    let asset = 2;
    let mut b = Book::new(asset);
    let mut r = RefBook::default();
    let mut next_oid: Oid = 1;
    let mut issued: Vec<Oid> = vec![];
    for (step, op) in ops.into_iter().enumerate() {
        match op {
            Op::Place { user, side, px: p, sz: s, tif, cloid, ro } => {
                let oid = next_oid;
                next_oid += 1;
                issued.push(oid);
                let tif = [Tif::Gtc, Tif::Ioc, Tif::Alo][tif as usize];
                let o = RestingOrder {
                    oid,
                    user: addr(user),
                    asset,
                    side: Side::from_is_buy(side),
                    limit_px: Px(p * 10_000_000),
                    sz: Sz(s * 1_000_000),
                    orig_sz: Sz(s * 1_000_000),
                    reduce_only: ro,
                    cloid: if cloid { Some(oid.to_be_bytes().repeat(2).try_into().unwrap()) } else { None },
                    timestamp: step as u64,
                    tif,
                };
                let mut h1 = ScriptHook { salt, mode, n: 0, log: vec![] };
                let mut h2 = ScriptHook { salt, mode, n: 0, log: vec![] };
                let o1 = b.place(o.clone(), &mut h1);
                let o2 = r.place(o, &mut h2);
                assert_eq!(h1.log, h2.log, "candidate sequence differs at step {step}");
                assert_eq!(o1, o2, "outcome differs at step {step}");
            }
            Op::Cancel { pick } => {
                if issued.is_empty() {
                    continue;
                }
                let oid = issued[pick % issued.len()];
                assert_eq!(b.cancel(oid), r.cancel(oid));
            }
            Op::Reduce { pick, frac } => {
                if issued.is_empty() {
                    continue;
                }
                let oid = issued[pick % issued.len()];
                let cur = r.orders.iter_mut().find(|(_, o)| o.oid == oid);
                match cur {
                    Some((_, o)) => {
                        let new = Sz((o.sz.0 * frac as i64 / 100).max(1));
                        o.sz = new;
                        assert!(b.reduce_size(oid, new));
                    }
                    None => assert!(!b.reduce_size(oid, Sz(1))),
                }
            }
        }
        b.check_invariants();
        for side in [Side::Bid, Side::Ask] {
            assert_eq!(b.levels(side, usize::MAX), r.levels(side));
            assert_eq!(b.levels(side, 3), r.levels(side).into_iter().take(3).collect::<Vec<_>>());
        }
        assert_eq!(b.best_bid(), r.levels(Side::Bid).first().map(|l| l.0));
        assert_eq!(b.best_ask(), r.levels(Side::Ask).first().map(|l| l.0));
        assert_eq!(b.len(), r.orders.len());
        for u in 0..5 {
            let mine: Vec<_> = b.user_orders(&addr(u)).into_iter().cloned().collect();
            let mut theirs: Vec<_> = r.orders.iter().map(|(_, o)| o.clone()).filter(|o| o.user == addr(u)).collect();
            theirs.sort_by_key(|o| o.oid);
            assert_eq!(mine, theirs);
        }
        for (_, o) in &r.orders {
            assert_eq!(b.get(o.oid), Some(o));
            if let Some(c) = o.cloid {
                assert_eq!(b.get_by_cloid(&o.user, &c), Some(o));
            }
        }
        let enc = b.encode();
        assert_eq!(enc, r.encode(asset));
        let d = Book::decode(&enc).unwrap();
        d.check_invariants();
        assert_eq!(d.encode(), enc);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 300, .. ProptestConfig::default() })]

    #[test]
    fn matches_reference_fill_all(ops in prop::collection::vec(op(), 1..120), salt in any::<u64>()) {
        run(ops, salt, 0);
    }

    #[test]
    fn matches_reference_scripted_hook(ops in prop::collection::vec(op(), 1..120), salt in any::<u64>()) {
        run(ops, salt, 1);
    }

    /// Decoded book behaves identically to the original for further ops.
    #[test]
    fn decoded_book_behaves_identically(ops in prop::collection::vec(op(), 1..60), more in prop::collection::vec(op(), 1..60), salt in any::<u64>()) {
        let mut b = Book::new(0);
        let mut oid = 1;
        let apply = |b: &mut Book, ops: &[Op], oid: &mut Oid| -> Vec<PlaceOutcome> {
            let mut outs = vec![];
            for op in ops {
                match op {
                    Op::Place { user, side, px: p, sz: s, tif, .. } => {
                        let mut o = order(*oid, *user, Side::from_is_buy(*side), *p, *s, [Tif::Gtc, Tif::Ioc, Tif::Alo][*tif as usize]);
                        o.timestamp = 0;
                        *oid += 1;
                        let mut h = ScriptHook { salt, mode: 1, n: 0, log: vec![] };
                        outs.push(b.place(o, &mut h));
                    }
                    Op::Cancel { pick } => { b.cancel((*pick as u64) % (*oid)); }
                    Op::Reduce { .. } => {}
                }
            }
            outs
        };
        apply(&mut b, &ops, &mut oid);
        let mut d = Book::decode(&b.encode()).unwrap();
        let mut oid2 = oid;
        let o1 = apply(&mut b, &more, &mut oid);
        let o2 = apply(&mut d, &more, &mut oid2);
        prop_assert_eq!(o1, o2);
        prop_assert_eq!(b.encode(), d.encode());
    }
}
