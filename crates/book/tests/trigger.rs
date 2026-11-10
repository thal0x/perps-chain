mod common;
use book::*;
use common::*;
use interfaces::*;
use proptest::prelude::*;

fn trig(oid: Oid, user: u8, side: Side, tpsl: TpSl, trigger: u64, parent: Option<Oid>) -> TriggerOrder {
    TriggerOrder {
        oid,
        user: addr(user),
        asset: 1,
        side,
        limit_px: px(trigger),
        sz: sz(1),
        reduce_only: true,
        cloid: None,
        timestamp: 0,
        trigger_px: px(trigger),
        is_market: true,
        tpsl,
        parent_oid: parent,
        is_position_tpsl: false,
    }
}

#[test]
fn directions() {
    use Side::*;
    use TpSl::*;
    assert!(fires_on_rise(&trig(1, 1, Ask, Tp, 100, None))); // long TP
    assert!(fires_on_rise(&trig(1, 1, Bid, Sl, 100, None))); // short SL
    assert!(!fires_on_rise(&trig(1, 1, Bid, Tp, 100, None))); // short TP
    assert!(!fires_on_rise(&trig(1, 1, Ask, Sl, 100, None))); // long SL
    let o = trig(1, 1, Ask, Tp, 100, None);
    assert!(condition_holds(&o, px(100)));
    assert!(condition_holds(&o, px(101)));
    assert!(!condition_holds(&o, px(99)));
    let o = trig(1, 1, Ask, Sl, 100, None);
    assert!(condition_holds(&o, px(100)));
    assert!(condition_holds(&o, px(99)));
    assert!(!condition_holds(&o, px(101)));
}

#[test]
fn take_triggered_by_condition_in_oid_order() {
    use Side::*;
    use TpSl::*;
    let mut t = TriggerBook::new(1);
    t.insert(trig(5, 1, Ask, Tp, 110, None)); // rise to 110
    t.insert(trig(2, 1, Ask, Sl, 90, None)); // fall to 90
    t.insert(trig(7, 2, Bid, Sl, 105, None)); // rise to 105
    t.insert(trig(3, 2, Bid, Tp, 95, None)); // fall to 95
    t.check_invariants();
    assert!(t.take_triggered(px(100)).is_empty());
    let got: Vec<_> = t.take_triggered(px(105)).into_iter().map(|o| o.oid).collect();
    assert_eq!(got, vec![7]);
    let got: Vec<_> = t.take_triggered(px(200)).into_iter().map(|o| o.oid).collect();
    assert_eq!(got, vec![5]);
    let got: Vec<_> = t.take_triggered(px(1)).into_iter().map(|o| o.oid).collect();
    assert_eq!(got, vec![2, 3]);
    assert!(t.is_empty());
    t.check_invariants();
}

#[test]
fn both_directions_same_mark_sorted_by_oid() {
    use Side::*;
    use TpSl::*;
    let mut t = TriggerBook::new(1);
    t.insert(trig(9, 1, Ask, Tp, 100, None));
    t.insert(trig(4, 1, Ask, Sl, 100, None));
    t.insert(trig(6, 1, Bid, Sl, 99, None));
    let got: Vec<_> = t.take_triggered(px(100)).into_iter().map(|o| o.oid).collect();
    assert_eq!(got, vec![4, 6, 9]);
}

#[test]
fn children_wait_for_parent() {
    use Side::*;
    use TpSl::*;
    let mut t = TriggerBook::new(1);
    t.insert(trig(11, 1, Ask, Tp, 110, Some(10)));
    t.insert(trig(12, 1, Ask, Sl, 90, Some(10)));
    t.insert(trig(21, 1, Ask, Tp, 110, Some(20)));
    t.check_invariants();
    assert_eq!(t.children_of(10), vec![11, 12]);
    // not eligible while parent unfilled
    assert!(t.take_triggered(px(1000)).is_empty());
    assert!(t.take_triggered(px(1)).is_empty());
    assert_eq!(t.release_children(10), vec![11, 12]);
    assert_eq!(t.release_children(10), Vec::<Oid>::new());
    assert_eq!(t.get(11).unwrap().parent_oid, None);
    t.check_invariants();
    let got: Vec<_> = t.take_triggered(px(1000)).into_iter().map(|o| o.oid).collect();
    assert_eq!(got, vec![11]);
    // parent 20 canceled -> child removed
    let removed: Vec<_> = t.remove_children(20).into_iter().map(|o| o.oid).collect();
    assert_eq!(removed, vec![21]);
    assert!(t.remove_children(20).is_empty());
    assert_eq!(t.len(), 1);
    t.check_invariants();
}

#[test]
fn remove_and_user_orders() {
    use Side::*;
    use TpSl::*;
    let mut t = TriggerBook::new(1);
    t.insert(trig(8, 1, Ask, Tp, 110, None));
    t.insert(trig(3, 1, Ask, Sl, 90, Some(2)));
    t.insert(trig(5, 2, Bid, Sl, 105, None));
    assert_eq!(t.user_orders(&addr(1)).iter().map(|o| o.oid).collect::<Vec<_>>(), vec![3, 8]);
    assert_eq!(t.remove(3).unwrap().oid, 3);
    assert!(t.remove(3).is_none());
    assert!(t.children_of(2).is_empty());
    assert_eq!(t.remove(8).unwrap().oid, 8);
    assert!(t.user_orders(&addr(1)).is_empty());
    assert!(t.take_triggered(px(200)).iter().all(|o| o.oid == 5));
    t.check_invariants();
    assert!(t.update_sz(5, sz(3)) == false);
}

#[test]
fn encode_roundtrip_canonical() {
    use Side::*;
    use TpSl::*;
    let mut a = TriggerBook::new(1);
    a.insert(trig(8, 1, Ask, Tp, 110, None));
    a.insert(trig(3, 1, Ask, Sl, 90, Some(2)));
    let mut b = TriggerBook::new(1);
    b.insert(trig(99, 1, Ask, Tp, 1, None));
    b.insert(trig(3, 1, Ask, Sl, 90, Some(2)));
    b.insert(trig(8, 1, Ask, Tp, 110, None));
    b.remove(99);
    assert_eq!(a.encode(), b.encode());
    let d = TriggerBook::decode(&a.encode()).unwrap();
    d.check_invariants();
    assert_eq!(d.encode(), a.encode());
    assert_eq!(d.children_of(2), vec![3]);
    assert!(TriggerBook::decode(&[1, 2]).is_err());
}

#[test]
#[should_panic(expected = "duplicate trigger oid")]
fn duplicate_insert_panics() {
    let mut t = TriggerBook::new(1);
    t.insert(trig(1, 1, Side::Ask, TpSl::Tp, 1, None));
    t.insert(trig(1, 1, Side::Ask, TpSl::Tp, 1, None));
}

proptest! {
    /// take_triggered agrees with a brute-force scan.
    #[test]
    fn take_triggered_matches_scan(
        orders in prop::collection::vec((any::<bool>(), any::<bool>(), 90u64..110, prop::option::of(0u64..5)), 0..60),
        released in prop::collection::vec(0u64..5, 0..3),
        marks in prop::collection::vec(85u64..115, 1..8),
    ) {
        let mut t = TriggerBook::new(1);
        let mut model: Vec<TriggerOrder> = vec![];
        for (i, (side, tp, p, parent)) in orders.into_iter().enumerate() {
            let o = trig(100 + i as u64, (i % 3) as u8, Side::from_is_buy(side), if tp { TpSl::Tp } else { TpSl::Sl }, p, parent);
            t.insert(o.clone());
            model.push(o);
        }
        for p in released {
            let mut want: Vec<Oid> = model.iter().filter(|o| o.parent_oid == Some(p)).map(|o| o.oid).collect();
            want.sort();
            for o in model.iter_mut().filter(|o| o.parent_oid == Some(p)) { o.parent_oid = None; }
            prop_assert_eq!(t.release_children(p), want);
        }
        for m in marks {
            let got = t.take_triggered(px(m));
            let mut want: Vec<TriggerOrder> = model.iter().filter(|o| o.parent_oid.is_none() && condition_holds(o, px(m))).cloned().collect();
            want.sort_by_key(|o| o.oid);
            model.retain(|o| !(o.parent_oid.is_none() && condition_holds(o, px(m))));
            prop_assert_eq!(got, want);
            t.check_invariants();
            prop_assert_eq!(t.len(), model.len());
        }
        let d = TriggerBook::decode(&t.encode()).unwrap();
        prop_assert_eq!(d.encode(), t.encode());
    }
}
