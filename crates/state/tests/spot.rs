//! Spot trading on PURR/USDC (asset 10000, D25).
mod common;
use common::*;
use interfaces::*;
use proptest::prelude::*;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;
use state::errors as msg;
use state::{ChainConfig, TestChain, SPOT_ASSET, SPOT_FEE_ACCOUNT};
use std::sync::Arc;

const S: AssetId = SPOT_ASSET;

fn spot_genesis(users: &[(u64, &str, &str)]) -> Genesis {
    let mut g = test_genesis(4, &[(addr(MM), usd("1000000"))]);
    for (u, q, b) in users {
        if *q != "0" {
            g.spot_balances.push((addr(*u), 0, usd(q)));
        }
        if *b != "0" {
            g.spot_balances.push((addr(*u), 1, usd(b)));
        }
    }
    g
}

fn setup() -> H {
    H::from_genesis(spot_genesis(&[(MM, "10000", "10000"), (ALICE, "1000", "1000"), (BOB, "1000", "0")]))
}

fn bal(h: &H, u: Address, t: u32) -> Usd {
    h.chain.clearinghouse().spot_balance(&u, t)
}

fn total(h: &H, t: u32) -> Usd {
    let mut s = Usd::ZERO;
    for a in h.chain.clearinghouse().addresses() {
        s += bal(h, a, t);
    }
    s
}

fn spot(is_buy: bool, p: &str, s: &str, tif: Tif) -> OrderRequest {
    limit(S, is_buy, p, s, tif)
}

#[test]
fn rest_query_hold_cancel() {
    let mut h = setup();
    let a = addr(ALICE);
    let st = h.order1(a, spot(true, "0.5", "24", Tif::Gtc));
    let oid = resting_oid(&st);
    // hold = 12 + ceil(12 * 0.035%) = 12.0042 USDC
    assert_eq!(h.chain.spot_hold(&a).quote, usd("12.0042"));
    assert_eq!(h.chain.spot_free(&a, 0), usd("987.9958"));
    let q = h.chain.q();
    assert_eq!(q.l2_book(S, 10).0, vec![(px("0.5"), sz("24"), 1)]);
    assert_eq!(q.open_orders(&a).len(), 1);
    assert_eq!(q.open_orders(&a)[0].asset, S);
    assert_eq!(q.order_by_oid(&a, oid).unwrap().limit_px, px("0.5"));
    assert!(h.last_events().iter().any(|e| matches!(e, Event::BookChanged { asset } if *asset == S)));
    // sell hold in base
    let st = h.order1(a, spot(false, "2", "10", Tif::Gtc));
    resting_oid(&st);
    assert_eq!(h.chain.spot_hold(&a).base, usd("10"));
    assert_eq!(h.chain.q().mid(S), Some(px("1.25")));
    assert_eq!(h.cancel(addr(BOB), S, oid), SimpleStatus::Error(msg::CANCEL_MISSING.into()));
    assert_eq!(h.cancel(a, S, oid), SimpleStatus::Success);
    assert_eq!(h.chain.spot_hold(&a).quote, Usd::ZERO);
    assert_eq!(h.chain.q().open_orders(&a).len(), 1);
}

#[test]
fn validation() {
    let mut h = setup();
    let a = addr(ALICE);
    let st = h.order(
        a,
        vec![
            spot(true, "0.123456", "100", Tif::Gtc),  // 6 sig figs
            spot(true, "1.000001", "100", Tif::Gtc), // 7 sig figs
            spot(true, "1", "10.5", Tif::Gtc),        // PURR szDecimals 0
            spot(true, "1", "9", Tif::Gtc),           // $9
            OrderRequest { reduce_only: true, ..spot(false, "1", "20", Tif::Gtc) },
            spot(true, "1", "2000", Tif::Gtc),        // > 1000 USDC
            spot(false, "1", "1001", Tif::Gtc),       // > 1000 PURR
            spot(true, "0.5", "20", Tif::Ioc),        // nothing to match
            trigger(S, false, "1", "20", "1", true, TpSl::Sl),
            limit(10001, true, "1", "20", Tif::Gtc),  // alias pair: no book
            spot(true, "0.00012345", "100000", Tif::Gtc), // 8 decimals, 5 sig figs: OK
        ],
    );
    assert_eq!(err_of(&st[0]), msg::INVALID_PRICE);
    assert_eq!(err_of(&st[1]), msg::INVALID_PRICE);
    assert_eq!(err_of(&st[2]), msg::INVALID_SIZE);
    assert_eq!(err_of(&st[3]), msg::MIN_VALUE);
    assert_eq!(err_of(&st[4]), msg::REDUCE_ONLY_INCREASE);
    assert_eq!(err_of(&st[5]), "Insufficient spot balance asset=10000");
    assert_eq!(err_of(&st[6]), "Insufficient spot balance asset=10000");
    assert_eq!(err_of(&st[7]), msg::IOC_NO_MATCH);
    assert_eq!(err_of(&st[8]), msg::SPOT_TRIGGER_UNSUPPORTED);
    assert_eq!(err_of(&st[9]), msg::INVALID_ASSET);
    resting_oid(&st[10]);
}

#[test]
fn fills_settle_balances_and_fees() {
    let mut h = setup();
    let (mm, a) = (addr(MM), addr(ALICE));
    let ask = resting_oid(&h.order1(mm, spot(false, "1.01", "100", Tif::Gtc)));
    resting_oid(&h.order1(mm, spot(false, "1.02", "100", Tif::Gtc)));
    assert_eq!(h.chain.spot_hold(&mm).base, usd("200"));
    let q0 = total(&h, 0);
    let b0 = total(&h, 1);
    // Alice buys 150 IOC up to 1.02: 100 @ 1.01 + 50 @ 1.02
    let st = h.order1(a, spot(true, "1.02", "150", Tif::Ioc));
    match st {
        OrderStatusResp::Filled { total_sz, avg_px, .. } => {
            assert_eq!(total_sz, sz("150"));
            assert_eq!(avg_px, px("1.01333333"));
        }
        other => panic!("{other:?}"),
    }
    let notional = usd("152"); // 101 + 51
    let fills: Vec<FillEvent> = h
        .last_events()
        .iter()
        .filter_map(|e| if let Event::Fill(f) = e { Some(f.clone()) } else { None })
        .collect();
    assert_eq!(fills.len(), 2);
    assert!(fills.iter().all(|f| f.asset == S && f.taker == a && f.maker == mm && f.taker_side == Side::Bid));
    assert_eq!(fills[0].maker_oid, ask);
    assert_eq!(fills[0].taker_fee, usd("0.03535"));
    assert_eq!(fills[0].maker_fee, usd("0.0101"));
    assert_eq!(fills[0].taker_start_pos, sz("1000"));
    assert_eq!(fills[1].taker_start_pos, sz("1100"));
    let tf: Usd = fills.iter().map(|f| f.taker_fee).fold(Usd::ZERO, |x, y| x + y);
    let mf: Usd = fills.iter().map(|f| f.maker_fee).fold(Usd::ZERO, |x, y| x + y);
    assert_eq!(bal(&h, a, 1), usd("1150"));
    assert_eq!(bal(&h, a, 0), usd("1000") - notional - tf);
    assert_eq!(bal(&h, mm, 1), usd("9850"));
    assert_eq!(bal(&h, mm, 0), usd("10000") + notional - mf);
    assert_eq!(bal(&h, SPOT_FEE_ACCOUNT, 0), tf + mf);
    assert_eq!(h.chain.spot_hold(&mm).base, usd("50"));
    assert_eq!(total(&h, 0), q0);
    assert_eq!(total(&h, 1), b0);
    assert_eq!(h.chain.spot_last_px(), Some(px("1.02")));
    let ctx = h.chain.q().market_ctx(S);
    assert_eq!(ctx.mark_px, px("1.02"));
    // maker fully filled order update
    assert!(h.last_events().iter().any(
        |e| matches!(e, Event::OrderUpdate { order, status: OrderLifecycle::Filled, .. } if order.oid == ask)
    ));
    // Bob sells into a resting GTC bid of Alice: Alice is maker.
    let bid = resting_oid(&h.order1(a, spot(true, "0.99", "20", Tif::Gtc)));
    let st = h.order1(addr(BOB), spot(false, "0.98", "20", Tif::Gtc));
    assert!(matches!(st, OrderStatusResp::Error(_)), "bob has no PURR: {st:?}");
    h.send_with_nonce(a, |n| Action::SpotSend {
        destination: addr(BOB),
        token: "PURR:0x11111111111111111111111111111111".into(),
        amount: usd("30"),
        time: n,
    })
    .unwrap();
    let st = h.order1(addr(BOB), spot(false, "0.98", "20", Tif::Gtc));
    assert!(matches!(st, OrderStatusResp::Filled { .. }), "{st:?}");
    assert!(h.chain.q().open_orders(&a).iter().all(|o| o.oid != bid));
    assert_eq!(h.chain.spot_hold(&a), state::SpotHold::default());
    assert_eq!(bal(&h, addr(BOB), 1), usd("10"));
    assert!(h.chain.check_invariants().is_ok());
}

#[test]
fn gtc_partial_rests_with_hold_and_alo() {
    let mut h = setup();
    let (mm, a) = (addr(MM), addr(ALICE));
    h.order1(mm, spot(false, "1", "30", Tif::Gtc));
    let st = h.order1(a, spot(true, "1", "50", Tif::Gtc));
    let oid = resting_oid(&st);
    assert_eq!(h.chain.q().order_by_oid(&a, oid).unwrap().sz, sz("20"));
    assert_eq!(h.chain.spot_hold(&a).quote, usd("20.007"));
    // ALO crossing is rejected with the bbo text
    let st = h.order1(mm, spot(false, "1", "20", Tif::Alo));
    assert_eq!(err_of(&st), msg::post_only_crossed("1", "None"));
    // held balance cannot be sent away
    let free = h.chain.spot_free(&a, 0);
    let r = h.send_with_nonce(a, |n| Action::SpotSend {
        destination: addr(BOB),
        token: "USDC".into(),
        amount: free + Usd::from_raw(1),
        time: n,
    });
    assert_eq!(r.unwrap_err(), "Insufficient balance for token transfer");
    let r = h.send_with_nonce(a, |n| Action::UsdClassTransfer { amount: free + Usd::from_raw(1), to_perp: true, nonce: n });
    assert_eq!(r.unwrap_err(), "Insufficient balance for transfer");
    h.send_with_nonce(a, |n| Action::UsdClassTransfer { amount: free, to_perp: true, nonce: n }).unwrap();
    assert_eq!(h.chain.spot_free(&a, 0), Usd::ZERO);
    assert!(h.chain.check_invariants().is_ok());
}

#[test]
fn self_trade_prevention() {
    let mut h = setup();
    let a = addr(ALICE);
    let own = resting_oid(&h.order1(a, spot(false, "1.5", "20", Tif::Gtc)));
    let st = h.order1(a, spot(true, "1.5", "20", Tif::Gtc));
    resting_oid(&st);
    assert!(h.last_events().iter().any(
        |e| matches!(e, Event::OrderUpdate { order, status: OrderLifecycle::SelfTradeCanceled, .. } if order.oid == own)
    ));
    assert_eq!(h.chain.spot_hold(&a).base, Usd::ZERO);
    assert_eq!(h.chain.spot_hold(&a).quote, usd("30.0105"));
}

#[test]
fn modify_batch_modify_cancel_by_cloid_schedule_cancel() {
    let mut h = setup();
    let a = addr(ALICE);
    let mut o = spot(true, "0.5", "40", Tif::Gtc);
    o.cloid = Some([7; 16]);
    let oid = resting_oid(&h.order1(a, o));
    // modify by oid: bigger size, hold follows
    let r = h.send(a, Action::Modify { oid: OidOrCloid::Oid(oid), order: spot(true, "0.6", "50", Tif::Gtc) });
    assert_eq!(r, Ok(ActionResponse::Default));
    let v = h.chain.q().order_by_cloid(&a, &[7; 16]).expect("cloid inherited");
    assert_eq!((v.limit_px, v.sz), (px("0.6"), sz("50")));
    assert_eq!(h.chain.spot_hold(&a).quote, usd("30.0105"));
    // modify that cannot be funded leaves the original
    let r = h.send(a, Action::Modify { oid: OidOrCloid::Cloid([7; 16]), order: spot(true, "0.6", "5000", Tif::Gtc) });
    assert_eq!(r, Err("Insufficient spot balance asset=10000".into()));
    assert_eq!(h.chain.q().order_by_cloid(&a, &[7; 16]).unwrap().oid, v.oid);
    // batchModify
    let r = h.send(a, Action::BatchModify { modifies: vec![(OidOrCloid::Oid(v.oid), spot(true, "0.7", "20", Tif::Gtc))] });
    assert!(matches!(r, Ok(ActionResponse::Order(ref s)) if matches!(s[0], OrderStatusResp::Resting { .. })), "{r:?}");
    // cancelByCloid
    let r = h.send(a, Action::CancelByCloid { cancels: vec![(S, [7; 16])] });
    assert_eq!(r, Ok(ActionResponse::Cancel(vec![SimpleStatus::Success])));
    assert!(h.chain.q().open_orders(&a).is_empty());
    // scheduled cancel cancels spot orders too
    resting_oid(&h.order1(a, spot(false, "3", "20", Tif::Gtc)));
    let t = h.time + 6000;
    h.send(a, Action::ScheduleCancel { time: Some(t) }).unwrap();
    for _ in 0..7 {
        h.block(vec![]);
    }
    assert!(h.chain.q().open_orders(&a).is_empty());
    assert_eq!(h.chain.spot_hold(&a), state::SpotHold::default());
}

#[test]
fn snapshot_restore_includes_spot() {
    let mut h = setup();
    h.order1(addr(MM), spot(false, "1.01", "100", Tif::Gtc));
    h.order1(addr(ALICE), spot(true, "0.95", "100", Tif::Gtc));
    h.order1(addr(ALICE), spot(true, "1.01", "10", Tif::Ioc));
    let root = h.chain.current_root();
    let snap = h.chain.snapshot();
    let mut other = TestChain::new(h.chain.genesis_ref().clone(), Arc::new(FakeVerifier), ChainConfig::strict());
    assert_eq!(other.restore(&snap).unwrap(), root);
    assert_eq!(other.spot_hold(&addr(ALICE)), h.chain.spot_hold(&addr(ALICE)));
    assert_eq!(other.query().l2_book(S, 5), h.chain.q().l2_book(S, 5));
    // a spot change changes the root
    let before = h.chain.current_root();
    h.order1(addr(MM), spot(false, "1.05", "10", Tif::Gtc));
    assert_ne!(before, h.chain.current_root());
}

// ------------------------------------------------------------- random ---

fn rand_order(rng: &mut ChaCha20Rng) -> OrderRequest {
    let is_buy = rng.gen_bool(0.5);
    let cents: u64 = rng.gen_range(90..=110);
    let p = format!("{}.{:02}", cents / 100, cents % 100);
    let s = rng.gen_range(10..=300).to_string();
    let tif = match rng.gen_range(0..10) {
        0..=5 => Tif::Gtc,
        6..=8 => Tif::Ioc,
        _ => Tif::Alo,
    };
    spot(is_buy, &p, &s, tif)
}

fn run_random(seed: u64, blocks: usize) -> (Vec<Hash32>, H) {
    let users = [MM, ALICE, BOB, 4, 5];
    let mut g = spot_genesis(&[(MM, "5000", "5000"), (ALICE, "800", "300"), (BOB, "300", "800"), (4, "50", "50"), (5, "2000", "0")]);
    g.params.maker_fee = Rate::from_raw(-100_000_000); // rebate path
    let mut h = H::from_genesis(g);
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let (q0, b0) = (total(&h, 0), total(&h, 1));
    let mut roots = vec![];
    for _ in 0..blocks {
        let mut txs = vec![];
        for _ in 0..rng.gen_range(1..6) {
            let u = addr(users[rng.gen_range(0..users.len())]);
            let n = h.next_nonce();
            let action = match rng.gen_range(0..10) {
                0..=6 => Action::Order { orders: (0..rng.gen_range(1..4)).map(|_| rand_order(&mut rng)).collect(), grouping: Grouping::Na },
                7 => {
                    let oids: Vec<Oid> = h.chain.q().open_orders(&u).iter().map(|o| o.oid).collect();
                    if oids.is_empty() {
                        Action::Noop
                    } else {
                        Action::Cancel { cancels: vec![(S, oids[rng.gen_range(0..oids.len())])] }
                    }
                }
                8 => {
                    let oids: Vec<Oid> = h.chain.q().open_orders(&u).iter().map(|o| o.oid).collect();
                    if oids.is_empty() {
                        Action::Noop
                    } else {
                        Action::Modify { oid: OidOrCloid::Oid(oids[0]), order: rand_order(&mut rng) }
                    }
                }
                _ => Action::SpotSend {
                    destination: addr(users[rng.gen_range(0..users.len())]),
                    token: if rng.gen_bool(0.5) { "USDC".into() } else { "PURR".into() },
                    amount: Usd::from_raw(rng.gen_range(1i128..200) * USD_SCALE),
                    time: n,
                },
            };
            txs.push(user_tx(u, &action, n));
        }
        h.block(txs);
        assert!(h.chain.check_invariants().is_ok(), "{:?}", h.chain.check_invariants());
        assert_eq!(total(&h, 0), q0, "USDC conserved");
        assert_eq!(total(&h, 1), b0, "PURR conserved");
        roots.push(h.chain.current_root());
    }
    (roots, h)
}

#[test]
fn random_spot_flow_is_deterministic_and_restorable() {
    let (r1, h1) = run_random(7, 60);
    let (r2, h2) = run_random(7, 60);
    assert_eq!(r1, r2);
    let fills = h1.all_events().filter(|e| matches!(e, Event::Fill(f) if f.asset == S)).count();
    assert!(fills > 10, "scenario should trade (got {fills} fills)");
    let e1: Vec<_> = h1.all_events().cloned().collect();
    let e2: Vec<_> = h2.all_events().cloned().collect();
    assert_eq!(e1, e2);
    let snap = h1.chain.snapshot();
    let mut other = TestChain::new(h1.chain.genesis_ref().clone(), Arc::new(FakeVerifier), ChainConfig::strict());
    assert_eq!(other.restore(&snap).unwrap(), *r1.last().unwrap());
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, .. ProptestConfig::default() })]
    /// Spot conservation: USDC (incl. fee account) and PURR totals are
    /// exact, holds == Σ order holds <= balances, every block.
    #[test]
    fn spot_conservation(seed in any::<u64>()) {
        run_random(seed, 25);
    }
}
