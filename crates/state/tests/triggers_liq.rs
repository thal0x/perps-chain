mod common;
use common::*;
use interfaces::*;

fn setup() -> H {
    H::new(&[(addr(MM), "10000000"), (addr(ALICE), "10000"), (addr(BOB), "1000")])
}

fn quote(h: &mut H, bid: &str, ask: &str, s: &str) -> (Oid, Oid) {
    let mm = addr(MM);
    let open: Vec<(AssetId, Oid)> = h.chain.q().open_orders(&mm).iter().map(|o| (o.asset, o.oid)).collect();
    if !open.is_empty() {
        h.send(mm, Action::Cancel { cancels: open }).unwrap();
    }
    let st = h.order(mm, vec![limit(0, true, bid, s, Tif::Gtc), limit(0, false, ask, s, Tif::Gtc)]);
    (resting_oid(&st[0]), resting_oid(&st[1]))
}

#[test]
fn stop_loss_triggers_and_closes() {
    let mut h = setup();
    let a = addr(ALICE);
    quote(&mut h, "59900", "60100", "0.1");
    assert_eq!(h.chain.mark_px(0), px("60000"));
    let st = h.order1(a, limit(0, true, "60100", "0.1", Tif::Ioc));
    assert!(matches!(st, OrderStatusResp::Filled { .. }));
    assert_eq!(h.pos(a, 0), sz("0.1"));
    // SL sell, market, reduce-only (basic_tpsl style) => reported as resting
    let sl = h.order1(a, trigger(0, false, "50000", "0.1", "59000", true, TpSl::Sl));
    let sl_oid = resting_oid(&sl);
    let v = h.chain.q().order_by_oid(&a, sl_oid).unwrap();
    assert!(v.is_trigger);
    assert_eq!(v.order_type, "Stop Market");
    // market drops: mark = median(60000, 58000, 58200) = 58200 <= 59000
    quote(&mut h, "58000", "58200", "1");
    assert_eq!(h.pos(a, 0), Sz::ZERO);
    let ev = h.last_events();
    assert!(ev.iter().any(|e| matches!(e, Event::OrderUpdate { order, status: OrderLifecycle::Triggered, .. } if order.oid == sl_oid)));
    let f = ev.iter().find_map(|e| if let Event::Fill(f) = e { Some(f) } else { None }).unwrap();
    assert_eq!((f.taker, f.taker_oid, f.px, f.sz), (a, sl_oid, px("58000"), sz("0.1")));
    assert!(h.chain.q().order_by_oid(&a, sl_oid).is_none());
}

#[test]
fn reduce_only_trigger_without_position_is_dropped_at_trigger_time() {
    let mut h = setup();
    let a = addr(ALICE);
    quote(&mut h, "59900", "60100", "0.1");
    let tp = resting_oid(&h.order1(a, trigger(0, false, "60000", "0.1", "60500", true, TpSl::Tp)));
    quote(&mut h, "60900", "61100", "0.1");
    assert!(h.chain.q().order_by_oid(&a, tp).is_none());
    assert!(h.last_events().iter().any(|e| matches!(e, Event::OrderUpdate { status: OrderLifecycle::ReduceOnlyCanceled, .. })));
    assert_eq!(h.pos(a, 0), Sz::ZERO);
}

#[test]
fn normal_tpsl_children_wait_for_parent_fill() {
    let mut h = setup();
    let (a, mm) = (addr(ALICE), addr(MM));
    quote(&mut h, "59900", "60100", "1");
    // parent rests below the ask => children wait for fill
    let st = h.order_grouped(
        a,
        vec![
            limit(0, true, "59950", "0.1", Tif::Gtc),
            trigger(0, false, "62000", "0.1", "62000", true, TpSl::Tp),
            trigger(0, false, "57000", "0.1", "58000", true, TpSl::Sl),
        ],
        Grouping::NormalTpsl,
    );
    let parent = resting_oid(&st[0]);
    let OrderStatusResp::WaitingForFill { oid: tp } = st[1] else { panic!("{st:?}") };
    let OrderStatusResp::WaitingForFill { oid: sl } = st[2] else { panic!("{st:?}") };
    assert_eq!(h.chain.triggers(0).get(tp).unwrap().parent_oid, Some(parent));
    // a sell fills the parent (as maker) -> children released
    let r = h.order1(mm, limit(0, false, "59950", "0.1", Tif::Ioc));
    assert!(matches!(r, OrderStatusResp::Filled { .. }));
    assert_eq!(h.pos(a, 0), sz("0.1"));
    assert_eq!(h.chain.triggers(0).get(tp).unwrap().parent_oid, None);
    assert_eq!(h.chain.triggers(0).get(sl).unwrap().parent_oid, None);
    // price falls through the SL => position closed
    quote(&mut h, "57500", "57700", "1");
    assert_eq!(h.pos(a, 0), Sz::ZERO);
    assert!(h.chain.triggers(0).get(sl).is_none());
    // TP still parked (reduce-only; dropped when it fires with no position)
    assert!(h.chain.triggers(0).get(tp).is_some());

    // Canceling an unfilled parent removes its children.
    let st = h.order_grouped(
        a,
        vec![limit(0, true, "57000", "0.1", Tif::Gtc), trigger(0, false, "60000", "0.1", "60000", true, TpSl::Tp)],
        Grouping::NormalTpsl,
    );
    let p2 = resting_oid(&st[0]);
    let OrderStatusResp::WaitingForFill { oid: c2 } = st[1] else { panic!() };
    assert_eq!(h.cancel(a, 0, p2), SimpleStatus::Success);
    assert!(h.chain.triggers(0).get(c2).is_none());

    // Immediately filled parent => children waiting for trigger.
    let st = h.order_grouped(
        a,
        vec![limit(0, true, "57700", "0.1", Tif::Gtc), trigger(0, false, "50000", "0.1", "55000", true, TpSl::Sl)],
        Grouping::NormalTpsl,
    );
    assert!(matches!(st[0], OrderStatusResp::Filled { .. }), "{st:?}");
    assert!(matches!(st[1], OrderStatusResp::WaitingForTrigger { .. }), "{st:?}");
}

#[test]
fn position_tpsl_follows_position() {
    let mut h = setup();
    let a = addr(ALICE);
    quote(&mut h, "59900", "60100", "1");
    h.order1(a, limit(0, true, "60100", "0.3", Tif::Ioc));
    let mut tp = trigger(0, false, "60000", "0", "61000", false, TpSl::Tp);
    tp.reduce_only = true;
    let st = h.order_grouped(a, vec![tp], Grouping::PositionTpsl);
    let OrderStatusResp::WaitingForTrigger { oid } = st[0] else { panic!("{st:?}") };
    // position grows after placement; TP closes all of it
    h.order1(a, limit(0, true, "60100", "0.2", Tif::Ioc));
    assert_eq!(h.pos(a, 0), sz("0.5"));
    quote(&mut h, "61200", "61400", "1");
    assert_eq!(h.pos(a, 0), Sz::ZERO);
    assert!(h.chain.triggers(0).get(oid).is_none());
    let st = h.order_grouped(a, vec![limit(0, true, "1000", "1", Tif::Gtc)], Grouping::PositionTpsl);
    assert!(matches!(st[0], OrderStatusResp::Error(_)));
}

#[test]
fn liquidation_through_book() {
    let mut h = setup();
    let b = addr(BOB);
    quote(&mut h, "59900", "60000", "1");
    h.send(b, Action::UpdateLeverage { asset: 0, is_cross: true, leverage: 50 }).unwrap();
    let st = h.order1(b, limit(0, true, "60000", "0.5", Tif::Ioc));
    assert!(matches!(st, OrderStatusResp::Filled { .. }), "{st:?}");
    // a resting order of Bob is canceled on liquidation
    resting_oid(&h.order1(b, limit(1, true, "1000", "0.01", Tif::Gtc)));
    quote(&mut h, "58200", "58400", "2");
    assert_eq!(h.pos(b, 0), Sz::ZERO);
    let ev = h.last_events();
    let f = ev
        .iter()
        .find_map(|e| if let Event::Fill(f) = e { if f.taker == b { Some(f) } else { None } } else { None })
        .expect("liquidation fill");
    assert_eq!(f.px, px("58200"));
    let li = f.liquidation.as_ref().unwrap();
    assert_eq!((li.liquidated_user, li.method), (b, LiquidationMethod::Market));
    assert!(ev.iter().any(|e| matches!(e, Event::OrderUpdate { status: OrderLifecycle::LiquidatedCanceled, .. })));
    assert!(ev.iter().any(|e| matches!(e, Event::Ledger { delta: LedgerDelta::Liquidation { .. }, .. })));
    assert!(h.chain.q().open_orders(&b).is_empty());
    assert!(h.chain.check_invariants().is_ok());
}

#[test]
fn liquidation_backstop_when_book_empty() {
    let mut h = setup();
    let (b, mm) = (addr(BOB), addr(MM));
    quote(&mut h, "59900", "60000", "1");
    h.send(b, Action::UpdateLeverage { asset: 0, is_cross: true, leverage: 50 }).unwrap();
    let st = h.order1(b, limit(0, true, "60000", "0.5", Tif::Ioc));
    assert!(matches!(st, OrderStatusResp::Filled { .. }), "{st:?}");
    let open: Vec<(AssetId, Oid)> = h.chain.q().open_orders(&mm).iter().map(|o| (o.asset, o.oid)).collect();
    h.send(mm, Action::Cancel { cancels: open }).unwrap();
    let ins = h.chain.clearinghouse().insurance_fund();
    h.oracle(0, "58000");
    assert_eq!(h.chain.mark_px(0), px("58000"));
    assert_eq!(h.pos(b, 0), Sz::ZERO);
    assert!(h.last_events().iter().any(|e| matches!(e, Event::BadDebtCovered { user, .. } if *user == b)));
    assert!(h.chain.clearinghouse().insurance_fund() < ins);
}
