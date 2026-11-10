mod common;
use book::*;
use common::*;
use interfaces::*;

use Side::{Ask, Bid};
use Tif::{Alo, Gtc, Ioc};

fn place(b: &mut Book, o: RestingOrder) -> PlaceOutcome {
    let out = b.place(o, &mut FillAll::default());
    b.check_invariants();
    out
}

#[test]
fn empty_book() {
    let b = Book::new(0);
    assert!(b.is_empty());
    assert_eq!(b.best_bid(), None);
    assert_eq!(b.best_ask(), None);
    assert!(b.levels(Bid, 10).is_empty());
    assert_eq!(b.asset(), 0);
    b.check_invariants();
}

#[test]
fn gtc_rests_without_cross() {
    let mut b = Book::new(0);
    let o = place(&mut b, order(1, 1, Bid, 100, 2, Gtc));
    assert_eq!(o.filled_sz, Sz::ZERO);
    assert_eq!(o.resting_sz, sz(2));
    assert_eq!(o.avg_px, None);
    assert!(!o.taker_canceled);
    let o = place(&mut b, order(2, 2, Ask, 101, 3, Gtc));
    assert_eq!(o.resting_sz, sz(3));
    assert_eq!(b.best_bid(), Some(px(100)));
    assert_eq!(b.best_ask(), Some(px(101)));
    assert_eq!(b.len(), 2);
    assert_eq!(b.get(1).unwrap().sz, sz(2));
}

#[test]
fn full_fill_at_maker_price() {
    let mut b = Book::new(0);
    place(&mut b, order(1, 1, Ask, 100, 2, Gtc));
    let mut h = FillAll::default();
    let o = b.place(order(2, 2, Bid, 105, 2, Gtc), &mut h);
    assert_eq!(o.filled_sz, sz(2));
    assert_eq!(o.avg_px, Some(px(100)));
    assert_eq!(o.resting_sz, Sz::ZERO);
    assert_eq!(o.filled_makers, vec![1]);
    assert!(!o.taker_canceled);
    assert!(b.is_empty());
    assert_eq!(h.seen, vec![(1, px(100), sz(2))]);
}

#[test]
fn partial_fill_gtc_rests_remainder() {
    let mut b = Book::new(0);
    place(&mut b, order(1, 1, Ask, 100, 2, Gtc));
    let o = place(&mut b, order(2, 2, Bid, 100, 5, Gtc));
    assert_eq!(o.filled_sz, sz(2));
    assert_eq!(o.resting_sz, sz(3));
    let r = b.get(2).unwrap();
    assert_eq!(r.sz, sz(3));
    assert_eq!(r.orig_sz, sz(5));
    assert_eq!(b.best_bid(), Some(px(100)));
    assert_eq!(b.best_ask(), None);
}

#[test]
fn partial_maker_keeps_priority() {
    let mut b = Book::new(0);
    place(&mut b, order(1, 1, Ask, 100, 5, Gtc));
    place(&mut b, order(2, 3, Ask, 100, 5, Gtc));
    let o = place(&mut b, order(3, 2, Bid, 100, 2, Ioc));
    assert_eq!(o.filled_sz, sz(2));
    assert_eq!(b.get(1).unwrap().sz, sz(3));
    // maker 1 still first
    let mut h = FillAll::default();
    b.place(order(4, 2, Bid, 100, 4, Ioc), &mut h);
    assert_eq!(h.seen, vec![(1, px(100), sz(3)), (2, px(100), sz(1))]);
    assert_eq!(b.get(2).unwrap().sz, sz(4));
    b.check_invariants();
}

#[test]
fn ioc_never_rests() {
    let mut b = Book::new(0);
    let o = place(&mut b, order(1, 1, Bid, 100, 2, Ioc));
    assert!(o.taker_canceled);
    assert_eq!(o.resting_sz, Sz::ZERO);
    assert!(b.is_empty());
    place(&mut b, order(2, 2, Ask, 100, 1, Gtc));
    let o = place(&mut b, order(3, 1, Bid, 100, 2, Ioc));
    assert_eq!(o.filled_sz, sz(1));
    assert!(o.taker_canceled);
    assert!(b.is_empty());
    // fully filled IOC is not "canceled"
    place(&mut b, order(4, 2, Ask, 100, 3, Gtc));
    let o = place(&mut b, order(5, 1, Bid, 100, 3, Ioc));
    assert!(!o.taker_canceled);
    assert_eq!(o.filled_sz, sz(3));
}

#[test]
fn alo_rejected_if_crossing_without_hook_calls() {
    let mut b = Book::new(0);
    place(&mut b, order(1, 1, Ask, 100, 2, Gtc));
    let mut h = FillAll::default();
    let o = b.place(order(2, 2, Bid, 100, 1, Alo), &mut h);
    assert!(o.alo_rejected);
    assert!(o.taker_canceled);
    assert_eq!(o.filled_sz, Sz::ZERO);
    assert!(h.seen.is_empty());
    assert!(b.get(2).is_none());
    // non-crossing ALO rests
    let o = place(&mut b, order(3, 2, Bid, 99, 1, Alo));
    assert!(!o.alo_rejected);
    assert_eq!(o.resting_sz, sz(1));
    assert_eq!(b.get(3).unwrap().tif, Alo);
    // ALO sell crossing the bid
    let o = place(&mut b, order(4, 3, Ask, 99, 1, Alo));
    assert!(o.alo_rejected);
    // ALO into empty side rests
    let mut b2 = Book::new(0);
    assert_eq!(place(&mut b2, order(1, 1, Ask, 1, 1, Alo)).resting_sz, sz(1));
}

#[test]
fn multi_level_sweep_and_avg_px_rounded_down() {
    let mut b = Book::new(0);
    place(&mut b, order(1, 1, Ask, 100, 1, Gtc));
    place(&mut b, order(2, 1, Ask, 101, 1, Gtc));
    place(&mut b, order(3, 1, Ask, 103, 1, Gtc));
    place(&mut b, order(4, 1, Ask, 110, 1, Gtc));
    let mut h = FillAll::default();
    let o = b.place(order(5, 2, Bid, 103, 10, Gtc), &mut h);
    assert_eq!(o.filled_sz, sz(3));
    // (100+101+103)/3 = 101.333.. -> floor at 1e-8
    assert_eq!(o.avg_px, Some(Px(10_133_333_333)));
    assert_eq!(o.filled_makers, vec![1, 2, 3]);
    assert_eq!(o.resting_sz, sz(7));
    assert_eq!(b.best_bid(), Some(px(103)));
    assert_eq!(b.best_ask(), Some(px(110)));
    assert_eq!(h.seen.iter().map(|s| s.1).collect::<Vec<_>>(), vec![px(100), px(101), px(103)]);
    b.check_invariants();
}

#[test]
fn sell_taker_sweeps_bids_high_to_low() {
    let mut b = Book::new(0);
    place(&mut b, order(1, 1, Bid, 98, 1, Gtc));
    place(&mut b, order(2, 1, Bid, 100, 1, Gtc));
    place(&mut b, order(3, 1, Bid, 99, 1, Gtc));
    let mut h = FillAll::default();
    let o = b.place(order(4, 2, Ask, 99, 5, Ioc), &mut h);
    assert_eq!(h.seen.iter().map(|s| s.0).collect::<Vec<_>>(), vec![2, 3]);
    assert_eq!(o.filled_sz, sz(2));
    assert_eq!(o.avg_px, Some(Px(9_950_000_000)));
    assert_eq!(b.best_bid(), Some(px(98)));
}

#[test]
fn time_priority_within_level() {
    let mut b = Book::new(0);
    for oid in [5, 3, 9, 1] {
        place(&mut b, order(oid, 1, Bid, 100, 1, Gtc));
    }
    let mut h = FillAll::default();
    b.place(order(10, 2, Ask, 100, 4, Ioc), &mut h);
    // arrival order, not oid order
    assert_eq!(h.seen.iter().map(|s| s.0).collect::<Vec<_>>(), vec![5, 3, 9, 1]);
}

#[test]
fn levels_aggregation() {
    let mut b = Book::new(0);
    place(&mut b, order(1, 1, Bid, 100, 1, Gtc));
    place(&mut b, order(2, 2, Bid, 100, 2, Gtc));
    place(&mut b, order(3, 1, Bid, 99, 4, Gtc));
    place(&mut b, order(4, 1, Bid, 97, 1, Gtc));
    place(&mut b, order(5, 1, Ask, 102, 3, Gtc));
    place(&mut b, order(6, 1, Ask, 101, 1, Gtc));
    assert_eq!(b.levels(Bid, 10), vec![(px(100), sz(3), 2), (px(99), sz(4), 1), (px(97), sz(1), 1)]);
    assert_eq!(b.levels(Bid, 2), vec![(px(100), sz(3), 2), (px(99), sz(4), 1)]);
    assert_eq!(b.levels(Ask, 10), vec![(px(101), sz(1), 1), (px(102), sz(3), 1)]);
    assert_eq!(b.levels(Ask, 0), vec![]);
    b.cancel(2);
    assert_eq!(b.levels(Bid, 1), vec![(px(100), sz(1), 1)]);
    // partial fill reduces level total
    place(&mut b, order(7, 3, Ask, 99, 3, Ioc));
    assert_eq!(b.levels(Bid, 10), vec![(px(99), sz(2), 1), (px(97), sz(1), 1)]);
}

#[test]
fn cancel_by_oid_and_middle_of_queue() {
    let mut b = Book::new(0);
    for oid in 1..=4 {
        place(&mut b, order(oid, oid as u8, Ask, 100, 1, Gtc));
    }
    let c = b.cancel(2).unwrap();
    assert_eq!(c.oid, 2);
    assert!(b.cancel(2).is_none());
    assert!(b.cancel(999).is_none());
    b.check_invariants();
    b.cancel(1);
    b.cancel(4);
    b.check_invariants();
    let mut h = FillAll::default();
    b.place(order(10, 9, Bid, 100, 5, Ioc), &mut h);
    assert_eq!(h.seen.iter().map(|s| s.0).collect::<Vec<_>>(), vec![3]);
    assert!(b.is_empty());
    b.check_invariants();
}

#[test]
fn cancel_then_replace_loses_priority() {
    // "modify" = cancel + new order: goes to the back of the queue
    let mut b = Book::new(0);
    place(&mut b, order(1, 1, Ask, 100, 1, Gtc));
    place(&mut b, order(2, 2, Ask, 100, 1, Gtc));
    let old = b.cancel(1).unwrap();
    let mut new = old.clone();
    new.oid = 3;
    new.sz = sz(2);
    new.orig_sz = sz(2);
    place(&mut b, new);
    let mut h = FillAll::default();
    b.place(order(4, 3, Bid, 100, 3, Ioc), &mut h);
    assert_eq!(h.seen.iter().map(|s| s.0).collect::<Vec<_>>(), vec![2, 3]);
}

#[test]
fn reduce_size_keeps_priority() {
    let mut b = Book::new(0);
    place(&mut b, order(1, 1, Ask, 100, 5, Gtc));
    place(&mut b, order(2, 2, Ask, 100, 5, Gtc));
    assert!(b.reduce_size(1, sz(1)));
    assert!(!b.reduce_size(77, sz(1)));
    b.check_invariants();
    assert_eq!(b.levels(Ask, 1), vec![(px(100), sz(6), 2)]);
    let mut h = FillAll::default();
    b.place(order(3, 3, Bid, 100, 2, Ioc), &mut h);
    assert_eq!(h.seen, vec![(1, px(100), sz(1)), (2, px(100), sz(1))]);
}

#[test]
fn cloid_lookup_per_user() {
    let mut b = Book::new(0);
    let c = [7u8; 16];
    let mut o = order(1, 1, Bid, 100, 1, Gtc);
    o.cloid = Some(c);
    place(&mut b, o);
    let mut o2 = order(2, 2, Bid, 99, 1, Gtc);
    o2.cloid = Some(c);
    place(&mut b, o2);
    assert_eq!(b.get_by_cloid(&addr(1), &c).unwrap().oid, 1);
    assert_eq!(b.get_by_cloid(&addr(2), &c).unwrap().oid, 2);
    assert!(b.get_by_cloid(&addr(3), &c).is_none());
    b.cancel(1);
    assert!(b.get_by_cloid(&addr(1), &c).is_none());
    // filled maker removes cloid entry
    place(&mut b, order(3, 3, Ask, 99, 1, Ioc));
    assert!(b.get_by_cloid(&addr(2), &c).is_none());
    // taker that fully fills is never indexed
    place(&mut b, order(4, 1, Ask, 100, 1, Gtc));
    let mut t = order(5, 2, Bid, 100, 1, Gtc);
    t.cloid = Some(c);
    place(&mut b, t);
    assert!(b.get_by_cloid(&addr(2), &c).is_none());
}

#[test]
fn user_orders_sorted_by_oid() {
    let mut b = Book::new(0);
    place(&mut b, order(9, 1, Bid, 90, 1, Gtc));
    place(&mut b, order(3, 1, Ask, 110, 1, Gtc));
    place(&mut b, order(5, 2, Bid, 95, 1, Gtc));
    place(&mut b, order(7, 1, Bid, 99, 1, Gtc));
    let oids: Vec<_> = b.user_orders(&addr(1)).iter().map(|o| o.oid).collect();
    assert_eq!(oids, vec![3, 7, 9]);
    assert!(b.user_orders(&addr(3)).is_empty());
    b.cancel(5);
    assert!(b.user_orders(&addr(2)).is_empty());
}

#[test]
fn hook_cancel_maker_continues() {
    let mut b = Book::new(0);
    place(&mut b, order(1, 1, Ask, 100, 1, Gtc));
    place(&mut b, order(2, 2, Ask, 100, 1, Gtc));
    place(&mut b, order(3, 3, Ask, 101, 1, Gtc));
    let mut h = FnHook(|c: &CandidateFill<'_>| {
        if c.maker.user == c.taker_user {
            (MatchDecision::CancelMaker(OrderLifecycle::SelfTradeCanceled), Sz::ZERO)
        } else {
            (MatchDecision::Fill, c.sz)
        }
    });
    let o = b.place(order(4, 2, Bid, 101, 3, Gtc), &mut h);
    b.check_invariants();
    assert_eq!(o.filled_sz, sz(2));
    assert_eq!(o.filled_makers, vec![1, 3]);
    assert_eq!(o.canceled_makers.len(), 1);
    assert_eq!(o.canceled_makers[0].0.oid, 2);
    assert_eq!(o.canceled_makers[0].1, OrderLifecycle::SelfTradeCanceled);
    assert_eq!(o.resting_sz, sz(1));
    assert_eq!(b.best_bid(), Some(px(101)));
    assert!(b.best_ask().is_none());
}

#[test]
fn hook_stop_taker_cancels_remainder_even_gtc() {
    let mut b = Book::new(0);
    place(&mut b, order(1, 1, Ask, 100, 1, Gtc));
    place(&mut b, order(2, 1, Ask, 100, 1, Gtc));
    let mut n = 0;
    let mut h = FnHook(|c: &CandidateFill<'_>| {
        n += 1;
        if n == 1 {
            (MatchDecision::Fill, c.sz)
        } else {
            (MatchDecision::StopTaker, Sz::ZERO)
        }
    });
    let o = b.place(order(3, 2, Bid, 100, 5, Gtc), &mut h);
    b.check_invariants();
    assert_eq!(o.filled_sz, sz(1));
    assert!(o.taker_canceled);
    assert_eq!(o.resting_sz, Sz::ZERO);
    assert!(b.get(3).is_none());
    assert!(b.get(2).is_some(), "maker untouched by StopTaker");
    assert!(o.canceled_makers.is_empty());
}

#[test]
fn hook_partial_accept_reoffers_same_maker() {
    let mut b = Book::new(0);
    place(&mut b, order(1, 1, Ask, 100, 4, Gtc));
    let mut offers = Vec::new();
    let mut h = FnHook(|c: &CandidateFill<'_>| {
        offers.push((c.maker.oid, c.sz));
        if offers.len() <= 2 {
            (MatchDecision::Fill, sz(1))
        } else {
            (MatchDecision::StopTaker, Sz::ZERO)
        }
    });
    let o = b.place(order(2, 2, Bid, 100, 3, Ioc), &mut h);
    assert_eq!(offers, vec![(1, sz(3)), (1, sz(2)), (1, sz(1))]);
    assert_eq!(o.filled_sz, sz(2));
    assert!(o.taker_canceled);
    assert_eq!(b.get(1).unwrap().sz, sz(2));
    b.check_invariants();
}

#[test]
fn hook_partial_accept_reduce_only_style() {
    // hook clamps the taker to 1.5 total across makers, then stops.
    let mut b = Book::new(0);
    place(&mut b, order(1, 1, Ask, 100, 1, Gtc));
    place(&mut b, order(2, 1, Ask, 101, 1, Gtc));
    let mut cap = Sz(150_000_000);
    let mut h = FnHook(move |c: &CandidateFill<'_>| {
        if cap.is_zero() {
            return (MatchDecision::StopTaker, Sz::ZERO);
        }
        let a = c.sz.min(cap);
        cap = cap - a;
        (MatchDecision::Fill, a)
    });
    let o = b.place(order(3, 2, Bid, 101, 5, Gtc), &mut h);
    assert_eq!(o.filled_sz, Sz(150_000_000));
    // (100*1 + 101*0.5)/1.5 = 100.3333...
    assert_eq!(o.avg_px, Some(Px(10_033_333_333)));
    assert_eq!(o.filled_makers, vec![1]);
    assert!(o.taker_canceled);
    assert_eq!(b.get(2).unwrap().sz, Sz(50_000_000));
    b.check_invariants();
}

#[test]
#[should_panic(expected = "hook accepted size")]
fn hook_overfill_panics() {
    let mut b = Book::new(0);
    b.place(order(1, 1, Ask, 100, 1, Gtc), &mut FillAll::default());
    let mut h = FnHook(|c: &CandidateFill<'_>| (MatchDecision::Fill, c.sz + sz(1)));
    b.place(order(2, 2, Bid, 100, 1, Gtc), &mut h);
}

#[test]
#[should_panic(expected = "duplicate oid")]
fn duplicate_oid_panics() {
    let mut b = Book::new(0);
    b.place(order(1, 1, Ask, 100, 1, Gtc), &mut FillAll::default());
    b.place(order(1, 1, Ask, 101, 1, Gtc), &mut FillAll::default());
}

#[test]
fn candidate_fields() {
    let mut b = Book::new(0);
    place(&mut b, order(1, 1, Ask, 100, 1, Gtc));
    let mut got = None;
    let mut h = FnHook(|c: &CandidateFill<'_>| {
        got = Some((c.taker_user, c.taker_oid, c.taker_side, c.taker_reduce_only, c.px, c.sz, c.maker.oid));
        (MatchDecision::Fill, c.sz)
    });
    let mut t = order(2, 2, Bid, 120, 3, Ioc);
    t.reduce_only = true;
    b.place(t, &mut h);
    assert_eq!(got, Some((addr(2), 2, Bid, true, px(100), sz(1), 1)));
}

#[test]
fn encode_decode_roundtrip_and_canonical() {
    let mut b = Book::new(3);
    let mk = |oid, side, p, s| {
        let mut o = order(oid, 1, side, p, s, Gtc);
        o.asset = 3;
        o
    };
    for (oid, side, p) in [(1, Bid, 100), (2, Bid, 99), (3, Ask, 101), (4, Bid, 100), (5, Ask, 103)] {
        place(&mut b, mk(oid, side, p, 2));
    }
    let bytes = b.encode();
    let d = Book::decode(&bytes).unwrap();
    d.check_invariants();
    assert_eq!(d.encode(), bytes);
    assert_eq!(d.levels(Bid, 10), b.levels(Bid, 10));

    // Same logical state via a different history (extra order placed and
    // canceled, different slab layout) encodes identically.
    let mut b2 = Book::new(3);
    place(&mut b2, mk(100, Ask, 200, 1));
    for (oid, side, p) in [(1, Bid, 100), (2, Bid, 99), (3, Ask, 101), (4, Bid, 100), (5, Ask, 103)] {
        place(&mut b2, mk(oid, side, p, 2));
    }
    b2.cancel(100);
    assert_eq!(b2.encode(), bytes);

    // FIFO order is semantic: swapping arrival order changes bytes.
    let mut b3 = Book::new(3);
    for (oid, side, p) in [(4, Bid, 100), (1, Bid, 100), (2, Bid, 99), (3, Ask, 101), (5, Ask, 103)] {
        place(&mut b3, mk(oid, side, p, 2));
    }
    assert_ne!(b3.encode(), bytes);

    assert!(Book::decode(&bytes[..bytes.len() - 1]).is_err());
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(Book::decode(&extra).is_err());
    let mut ver = bytes.clone();
    ver[0] = 9;
    assert_eq!(Book::decode(&ver).unwrap_err(), BookDecodeError::BadVersion(9));
}

#[test]
fn decode_rejects_crossed_or_unsorted() {
    #[derive(borsh::BorshSerialize)]
    struct Img {
        version: u8,
        asset: AssetId,
        bids: Vec<RestingOrder>,
        asks: Vec<RestingOrder>,
    }
    let enc = |bids, asks| borsh::to_vec(&Img { version: 1, asset: 0, bids, asks }).unwrap();
    let ok = enc(vec![order(1, 1, Bid, 100, 1, Gtc), order(2, 1, Bid, 99, 1, Gtc)], vec![]);
    assert!(Book::decode(&ok).is_ok());
    let unsorted = enc(vec![order(1, 1, Bid, 99, 1, Gtc), order(2, 1, Bid, 100, 1, Gtc)], vec![]);
    assert!(Book::decode(&unsorted).is_err());
    let crossed = enc(vec![order(1, 1, Bid, 100, 1, Gtc)], vec![order(2, 1, Ask, 100, 1, Gtc)]);
    assert!(Book::decode(&crossed).is_err());
    let wrong_side = enc(vec![order(1, 1, Ask, 100, 1, Gtc)], vec![]);
    assert!(Book::decode(&wrong_side).is_err());
    let dup = enc(vec![order(1, 1, Bid, 100, 1, Gtc)], vec![order(1, 1, Ask, 101, 1, Gtc)]);
    assert!(Book::decode(&dup).is_err());
    let ioc = enc(vec![order(1, 1, Bid, 100, 1, Ioc)], vec![]);
    assert!(Book::decode(&ioc).is_err());
}

#[test]
fn slab_slots_are_reused() {
    let mut b = Book::new(0);
    for i in 0..1000u64 {
        place(&mut b, order(i, 1, Bid, 100 + (i % 7), 1, Gtc));
        b.cancel(i);
    }
    assert!(b.is_empty());
    b.check_invariants();
}

#[test]
fn notional_acc() {
    let mut a = NotionalAcc::default();
    assert_eq!(a.avg_px(), None);
    a.add(Px(3), Sz(1));
    a.add(Px(4), Sz(1));
    assert_eq!(a.avg_px(), Some(Px(3))); // 3.5 -> 3
    assert_eq!(a.filled, 2);
    assert_eq!(a.px_sz_sum, 7);
}
