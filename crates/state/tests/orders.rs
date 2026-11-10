mod common;
use common::*;
use interfaces::*;
use state::errors as msg;

fn setup() -> H {
    H::new(&[(addr(MM), "1000000"), (addr(ALICE), "10000"), (addr(BOB), "10000")])
}

#[test]
fn resting_order_query_and_cancel() {
    let mut h = setup();
    let a = addr(ALICE);
    let mut o = limit(1, true, "1100", "0.2", Tif::Gtc);
    o.cloid = Some([1u8; 16]);
    let st = h.order1(a, o);
    let oid = resting_oid(&st);
    assert!(matches!(st, OrderStatusResp::Resting { cloid: Some(c), .. } if c == [1u8; 16]));
    let q = h.chain.q();
    assert_eq!(q.open_orders(&a).len(), 1);
    assert_eq!(q.order_by_oid(&a, oid).unwrap().limit_px, px("1100"));
    assert_eq!(q.order_by_cloid(&a, &[1u8; 16]).unwrap().oid, oid);
    assert_eq!(q.l2_book(1, 10).0, vec![(px("1100"), sz("0.2"), 1)]);
    assert!(h.last_events().iter().any(|e| matches!(e, Event::OrderUpdate { status: OrderLifecycle::Open, .. })));
    assert!(h.last_events().iter().any(|e| matches!(e, Event::BookChanged { asset: 1 })));
    // Someone else cannot cancel it.
    assert_eq!(h.cancel(addr(BOB), 1, oid), SimpleStatus::Error(msg::CANCEL_MISSING.into()));
    assert_eq!(h.cancel(a, 1, oid), SimpleStatus::Success);
    assert_eq!(h.cancel(a, 1, oid), SimpleStatus::Error("Order was never placed, already canceled, or filled.".into()));
    assert!(h.chain.q().open_orders(&a).is_empty());
}

#[test]
fn validation_errors() {
    let mut h = setup();
    let a = addr(ALICE);
    // ETH: sz_decimals 4 => px max 2 decimals, 5 sig figs.
    let bad = vec![
        limit(1, true, "1100.123", "0.2", Tif::Gtc),
        limit(1, true, "1234.56", "0.2", Tif::Gtc), // 6 sig figs
        limit(1, true, "1100", "0.00001", Tif::Gtc),
        limit(1, true, "1100", "0.001", Tif::Gtc), // $1.1
        limit(1, true, "1100", "0.2", Tif::Ioc),
        limit(1, true, "3000", "100", Tif::Gtc), // $300k on $10k at 25x
        OrderRequest { reduce_only: true, ..limit(1, false, "1100", "0.2", Tif::Gtc) },
        limit(9, true, "1100", "0.2", Tif::Gtc),
        limit(1, true, "12345", "0.01", Tif::Gtc), // integer px with 5 sig figs OK
    ];
    let st = h.order(a, bad);
    assert_eq!(err_of(&st[0]), "Order has invalid price.");
    assert_eq!(err_of(&st[1]), "Order has invalid price.");
    assert_eq!(err_of(&st[2]), "Order has invalid size.");
    assert_eq!(err_of(&st[3]), "Order must have minimum value of $10.");
    assert_eq!(err_of(&st[4]), "Order could not immediately match against any resting orders.");
    assert_eq!(err_of(&st[5]), "Insufficient margin to place order.");
    assert_eq!(err_of(&st[6]), "Reduce only order would increase position.");
    assert_eq!(err_of(&st[7]), msg::INVALID_ASSET);
    resting_oid(&st[8]);
}

#[test]
fn fills_events_and_positions() {
    let mut h = setup();
    let (mm, a) = (addr(MM), addr(ALICE));
    let st = h.order(
        mm,
        vec![limit(0, false, "60000", "0.1", Tif::Gtc), limit(0, false, "60010", "0.1", Tif::Gtc)],
    );
    let (m1, m2) = (resting_oid(&st[0]), resting_oid(&st[1]));
    let st = h.order1(a, limit(0, true, "60010", "0.15", Tif::Ioc));
    let OrderStatusResp::Filled { total_sz, avg_px, oid, .. } = st else { panic!("{st:?}") };
    assert_eq!(total_sz, sz("0.15"));
    assert_eq!(avg_px, px("60003.33333333"));
    assert!(oid > m2);
    let fills: Vec<&FillEvent> =
        h.last_events().iter().filter_map(|e| if let Event::Fill(f) = e { Some(f) } else { None }).collect();
    assert_eq!(fills.len(), 2);
    assert_eq!((fills[0].maker_oid, fills[0].px, fills[0].sz), (m1, px("60000"), sz("0.1")));
    assert_eq!((fills[1].maker_oid, fills[1].px, fills[1].sz), (m2, px("60010"), sz("0.05")));
    assert_eq!(fills[1].tid, fills[0].tid + 1);
    assert_eq!(fills[0].taker_side, Side::Bid);
    assert!(fills[0].taker_fee.0 > 0);
    assert_ne!(fills[0].tx_hash, Hash32::ZERO);
    assert_eq!(h.pos(a, 0), sz("0.15"));
    assert_eq!(h.pos(mm, 0), sz("-0.15"));
    // m1 fully filled => Filled update, m2 still open with 0.05
    assert!(h.last_events().iter().any(
        |e| matches!(e, Event::OrderUpdate { order, status: OrderLifecycle::Filled, .. } if order.oid == m1)
    ));
    assert_eq!(h.chain.q().order_by_oid(&mm, m2).unwrap().sz, sz("0.05"));
    // GTC that partially fills rests the remainder.
    let st = h.order1(a, limit(0, true, "60010", "0.1", Tif::Gtc));
    let oid = resting_oid(&st);
    assert_eq!(h.chain.q().order_by_oid(&a, oid).unwrap().sz, sz("0.05"));
    assert_eq!(h.chain.q().order_by_oid(&a, oid).unwrap().orig_sz, sz("0.1"));
    // Reduce-only sell larger than position is clamped and fills only the position.
    h.order1(mm, limit(0, true, "59000", "1", Tif::Gtc));
    let st = h.order1(a, OrderRequest { reduce_only: true, ..limit(0, false, "59000", "1", Tif::Ioc) });
    assert!(matches!(st, OrderStatusResp::Filled { total_sz, .. } if total_sz == sz("0.2")), "{st:?}");
    assert_eq!(h.pos(a, 0), Sz::ZERO);
}

#[test]
fn post_only_and_self_trade() {
    let mut h = setup();
    let (mm, a) = (addr(MM), addr(ALICE));
    h.order(mm, vec![limit(1, true, "2999", "1", Tif::Gtc), limit(1, false, "3001", "1", Tif::Gtc)]);
    let st = h.order1(a, limit(1, true, "3001", "1", Tif::Alo));
    assert_eq!(err_of(&st), "Post only order would have immediately matched, bbo was 2999@3001");
    resting_oid(&h.order1(a, limit(1, true, "3000", "1", Tif::Alo)));
    // MM crossing its own ask: STP cancels the resting maker, IOC finds nothing else.
    let st = h.order1(mm, limit(1, true, "3001", "1", Tif::Ioc));
    assert_eq!(err_of(&st), "Order could not immediately match against any resting orders.");
    assert!(h.last_events().iter().any(|e| matches!(e, Event::OrderUpdate { status: OrderLifecycle::SelfTradeCanceled, .. })));
    assert!(h.chain.q().l2_book(1, 5).1.is_empty());
    assert_eq!(h.pos(mm, 1), Sz::ZERO);
}

#[test]
fn maker_margin_recheck_cancels_maker() {
    let mut h = setup();
    let (a, b) = (addr(ALICE), addr(BOB));
    // Alice rests a large bid, then sends away most of her collateral.
    let oid = resting_oid(&h.order1(a, limit(1, true, "2900", "50", Tif::Gtc)));
    let r = h.send_with_nonce(a, |n| Action::UsdSend { destination: b, amount: usd("9900"), time: n });
    assert_eq!(r, Ok(ActionResponse::Default));
    let st = h.order1(b, limit(1, false, "2900", "10", Tif::Ioc));
    assert_eq!(err_of(&st), msg::IOC_NO_MATCH);
    assert!(h.last_events().iter().any(
        |e| matches!(e, Event::OrderUpdate { order, status: OrderLifecycle::MarginCanceled, .. } if order.oid == oid)
    ));
}

#[test]
fn cancel_by_cloid_modify_and_batch_modify() {
    let mut h = setup();
    let a = addr(ALICE);
    let cloid = [7u8; 16];
    let oid = resting_oid(&h.order1(a, OrderRequest { cloid: Some(cloid), ..limit(1, true, "1100", "0.2", Tif::Gtc) }));
    // modify by oid (HL basic_order_modify): new size/price, new oid, cloid kept.
    let r = h.send(a, Action::Modify { oid: OidOrCloid::Oid(oid), order: OrderRequest { cloid: Some(cloid), ..limit(1, true, "1105", "0.1", Tif::Gtc) } });
    assert_eq!(r, Ok(ActionResponse::Default));
    let q = h.chain.q();
    assert!(q.order_by_oid(&a, oid).is_none());
    let now = q.order_by_cloid(&a, &cloid).unwrap();
    assert_eq!((now.limit_px, now.sz), (px("1105"), sz("0.1")));
    assert!(now.oid > oid);
    // modify by cloid without cloid on the new order: cloid inherited.
    let r = h.send(a, Action::Modify { oid: OidOrCloid::Cloid(cloid), order: limit(1, true, "1106", "0.1", Tif::Gtc) });
    assert_eq!(r, Ok(ActionResponse::Default));
    assert_eq!(h.chain.q().order_by_cloid(&a, &cloid).unwrap().limit_px, px("1106"));
    // modify of a missing order
    let r = h.send(a, Action::Modify { oid: OidOrCloid::Oid(999_999), order: limit(1, true, "1106", "0.1", Tif::Gtc) });
    assert_eq!(r, Err("Cannot modify canceled or filled order".into()));
    // invalid replacement leaves the original in place
    let r = h.send(a, Action::Modify { oid: OidOrCloid::Cloid(cloid), order: limit(1, true, "1106.123", "0.1", Tif::Gtc) });
    assert_eq!(r, Err(msg::INVALID_PRICE.into()));
    assert!(h.chain.q().order_by_cloid(&a, &cloid).is_some());
    // batchModify
    let o2 = resting_oid(&h.order1(a, limit(1, true, "1000", "0.2", Tif::Gtc)));
    let r = h.send(
        a,
        Action::BatchModify {
            modifies: vec![
                (OidOrCloid::Oid(o2), limit(1, true, "1001", "0.2", Tif::Gtc)),
                (OidOrCloid::Oid(424242), limit(1, true, "1001", "0.2", Tif::Gtc)),
            ],
        },
    );
    let Ok(ActionResponse::Order(st)) = r else { panic!("{r:?}") };
    resting_oid(&st[0]);
    assert_eq!(err_of(&st[1]), msg::MODIFY_MISSING);
    // cancelByCloid
    let r = h.send(a, Action::CancelByCloid { cancels: vec![(1, cloid), (1, [9u8; 16])] });
    assert_eq!(
        r,
        Ok(ActionResponse::Cancel(vec![SimpleStatus::Success, SimpleStatus::Error(msg::CANCEL_MISSING.into())]))
    );
    assert_eq!(h.chain.q().open_orders(&a).len(), 1);
}

#[test]
fn schedule_cancel_dead_mans_switch() {
    let mut h = setup();
    let a = addr(ALICE);
    resting_oid(&h.order1(a, limit(1, true, "1100", "0.2", Tif::Gtc)));
    let t = h.time + 1000 + 4000;
    assert_eq!(h.send(a, Action::ScheduleCancel { time: Some(t) }), Err(msg::SCHEDULE_TOO_EARLY.into()));
    let t = h.time + 1000 + 10_000;
    assert_eq!(h.send(a, Action::ScheduleCancel { time: Some(t) }), Ok(ActionResponse::Default));
    assert_eq!(h.chain.user_meta(&a).unwrap().scheduled_cancel, Some(t));
    for _ in 0..8 {
        h.block(vec![]);
    }
    assert_eq!(h.chain.q().open_orders(&a).len(), 1);
    h.block(vec![]);
    h.block(vec![]);
    assert!(h.chain.q().open_orders(&a).is_empty());
    assert!(h.all_events().any(|e| matches!(e, Event::OrderUpdate { status: OrderLifecycle::ScheduledCancel, .. })));
    assert_eq!(h.chain.user_meta(&a).unwrap().scheduled_cancel, None);
    // Clearing works too.
    resting_oid(&h.order1(a, limit(1, true, "1100", "0.2", Tif::Gtc)));
    let t = h.time + 20_000;
    h.send(a, Action::ScheduleCancel { time: Some(t) }).unwrap();
    h.send(a, Action::ScheduleCancel { time: None }).unwrap();
    for _ in 0..25 {
        h.block(vec![]);
    }
    assert_eq!(h.chain.q().open_orders(&a).len(), 1);
}

#[test]
fn order_action_count_and_unknown_user() {
    let mut h = setup();
    let o = h.block(vec![user_tx(
        addr(ALICE),
        &Action::Order { orders: vec![limit(1, true, "1100", "0.2", Tif::Gtc); 3], grouping: Grouping::Na },
        h.time + 5,
    )]);
    assert_eq!(o.order_actions, 3);
    // A signer without account/meta does not exist.
    let r = h.send(addr(77), Action::Noop);
    assert_eq!(r, Err(format!("User or API Wallet {} does not exist.", addr(77))));
}
