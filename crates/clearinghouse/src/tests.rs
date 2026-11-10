//! Unit tests with hand-computed numbers.

use crate::*;
use interfaces::components::Clearinghouse as Api;

pub(crate) fn addr(n: u8) -> Address {
    Address([n; 20])
}
fn usd(s: &str) -> Usd {
    Usd::parse(s).unwrap()
}
fn px(s: &str) -> Px {
    Px::parse(s).unwrap()
}
fn sz(s: &str) -> Sz {
    Sz::parse(s).unwrap()
}

const T0: u64 = 1_700_000_000_000;

pub(crate) fn genesis(insurance: &str) -> Genesis {
    let m = |asset, name: &str, szd, lev, iso, p: &str| MarketSpec {
        asset,
        name: name.into(),
        sz_decimals: szd,
        max_leverage: lev,
        only_isolated: iso,
        initial_px: px(p),
    };
    Genesis {
        chain_id: 1,
        is_mainnet: false,
        genesis_time_ms: T0,
        validators: ValidatorSet { validators: vec![] },
        markets: vec![
            m(0, "BTC", 5, 50, false, "100000"),
            m(1, "ETH", 4, 25, false, "2000"),
            m(2, "ISO", 2, 10, true, "10"),
        ],
        balances: vec![],
        spot_tokens: vec![
            SpotToken { index: 0, name: "USDC".into(), sz_decimals: 8, wei_decimals: 8, token_id: [0; 16] },
            SpotToken { index: 1, name: "PURR".into(), sz_decimals: 0, wei_decimals: 5, token_id: [1; 16] },
        ],
        spot_balances: vec![(addr(9), 0, usd("500")), (addr(9), 1, usd("1000"))],
        params: ChainParams { insurance_fund: usd(insurance), ..Default::default() },
        bridge: BridgeParams { eth_chain_id: 31337, contract: Address::ZERO, min_deposit: Usd::ZERO },
    }
}

struct H {
    ch: Clearinghouse,
    flows: ExternalFlows,
}

impl H {
    fn new(insurance: &str) -> Self {
        let g = genesis(insurance);
        let flows = ExternalFlows { genesis: Clearinghouse::genesis_total(&g), ..Default::default() };
        let h = Self { ch: Clearinghouse::new(&g), flows };
        h.inv();
        h
    }
    fn inv(&self) {
        self.ch.check_invariants(&self.flows, 0).unwrap();
    }
    fn dep(&mut self, a: u8, amt: &str) {
        self.ch.deposit(addr(a), usd(amt)).unwrap();
        self.flows.deposits += usd(amt);
        self.inv();
    }
    fn fill(&mut self, taker: u8, maker: u8, buy: bool, p: &str, s: &str, asset: AssetId) -> Result<FillResult, ExecError> {
        let r = self.ch.apply_fill(&FillParams {
            asset,
            px: px(p),
            sz: sz(s),
            taker: addr(taker),
            maker: addr(maker),
            taker_side: Side::from_is_buy(buy),
            liquidation_of: None,
            time: T0,
        });
        self.inv();
        r
    }
    fn price(&mut self, asset: AssetId, p: &str) {
        self.ch.set_prices(asset, px(p), px(p), None, T0);
        self.inv();
    }
    fn cash(&self, a: u8) -> Usd {
        self.ch.account(&addr(a)).map(|s| s.cash).unwrap_or(Usd::ZERO)
    }
    fn view(&self, a: u8) -> AccountView {
        self.ch.account_view(&addr(a))
    }
}

#[test]
fn open_long_cross_fees_and_margin() {
    let mut h = H::new("0");
    h.dep(1, "10000");
    h.dep(2, "10000");
    let r = h.fill(1, 2, true, "100000", "0.1", 0).unwrap();
    // n = 10_000; taker 0.035% = 3.5 (ceil), maker 0.01% = 1
    assert_eq!(r.taker_fee, usd("3.5"));
    assert_eq!(r.maker_fee, usd("1"));
    assert_eq!(r.taker_start_pos, Sz::ZERO);
    assert_eq!(h.cash(1), usd("-3.5"));
    assert_eq!(h.cash(2), usd("19999"));
    assert_eq!(h.ch.ledger().fee_pool, usd("4.5"));
    let v = h.view(1);
    assert_eq!(v.positions.len(), 1);
    let p = &v.positions[0];
    assert_eq!(p.szi, sz("0.1"));
    assert_eq!(p.entry_px, px("100000"));
    assert_eq!(p.position_value, usd("10000"));
    assert_eq!(p.unrealized_pnl, Usd::ZERO);
    assert_eq!(p.leverage, Leverage::Cross(20));
    assert_eq!(p.margin_used, usd("500"));
    assert_eq!(v.margin_summary.account_value, usd("9996.5"));
    assert_eq!(v.cross_margin_summary.total_raw_usd, usd("-3.5"));
    assert_eq!(v.cross_maintenance_margin_used, usd("100")); // 1/(2*50)
    assert_eq!(v.withdrawable, usd("9496.5"));
    // liquidation px: equity − MM = 9896.5 over 0.1*(1−0.01) => 100000 − 99964.64.. = 35.35..
    let lp = p.liquidation_px.unwrap();
    assert_eq!(lp, Px::from_raw(100_000 * 100_000_000 - 9_996_464_646_464));
    assert_eq!(h.ch.market_ctx(0).open_interest, sz("0.1"));
    // price up 10%: uPnL 1000, ROE = 1000/(10000/20) = 2
    h.price(0, "110000");
    let p = &h.view(1).positions[0];
    assert_eq!(p.unrealized_pnl, usd("1000"));
    assert_eq!(p.return_on_equity, Rate::from_raw(2 * RATE_SCALE));
}

#[test]
fn margin_check_rejects_oversized_order() {
    let mut h = H::new("0");
    h.dep(1, "1000");
    h.dep(2, "100000");
    // 1000 at 20x => max ~20k notional; 0.2 BTC = 20000 + fee 7 > available
    let e = h.fill(1, 2, true, "100000", "0.2", 0).unwrap_err();
    assert_eq!(e.0, ERR_MARGIN);
    let before = h.ch.encode();
    assert!(h.fill(2, 1, false, "100000", "0.2", 0).is_err()); // maker side fails too
    assert_eq!(before, h.ch.encode(), "failed fill must not change state");
    // 0.19 = 19000, IM 950 + fee 6.65 <= 1000
    assert!(h
        .ch
        .check_order_margin(&MarginCheck { user: addr(1), asset: 0, side: Side::Bid, px: px("100000"), sz: sz("0.19"), reduce_only: false })
        .is_ok());
    assert!(h
        .ch
        .check_order_margin(&MarginCheck { user: addr(1), asset: 0, side: Side::Bid, px: px("100000"), sz: sz("0.2"), reduce_only: false })
        .is_err());
    h.fill(1, 2, true, "100000", "0.19", 0).unwrap();
    // reducing is always allowed even when under initial margin
    h.price(0, "96000");
    assert!(h.ch.withdrawable(&addr(1)).is_zero());
    assert!(h.fill(1, 2, true, "96000", "0.01", 0).is_err());
    h.fill(1, 2, false, "96000", "0.01", 0).unwrap();
    assert_eq!(h.ch.reduce_only_capacity(&addr(1), 0, Side::Ask), sz("0.18"));
    assert_eq!(h.ch.reduce_only_capacity(&addr(1), 0, Side::Bid), Sz::ZERO);
}

#[test]
fn realized_pnl_weighted_entry_partial_close_and_flip() {
    let mut h = H::new("0");
    h.dep(1, "100000");
    h.dep(2, "100000");
    h.dep(3, "100000");
    h.fill(1, 2, true, "2000", "1", 1).unwrap();
    h.fill(1, 2, true, "2020", "1", 1).unwrap();
    let p = &h.view(1).positions[0];
    assert_eq!(p.entry_px, px("2010"));
    // sell 0.5 @ 2100: removed basis 4020*0.5/2 = 1005, proceeds 1050 => +45
    let r = h.fill(1, 3, false, "2100", "0.5", 1).unwrap();
    assert_eq!(r.taker_closed_pnl, usd("45"));
    assert_eq!(r.taker_start_pos, sz("2"));
    assert_eq!(h.view(1).positions[0].entry_px, px("2010"));
    // maker 3 opened a short: no pnl
    assert_eq!(r.maker_closed_pnl, Usd::ZERO);
    // flip: sell 3 @ 1900: close 1.5 (basis 3015) for 2850 => -165; open short 1.5 @ 1900
    let r = h.fill(1, 3, false, "1900", "3", 1).unwrap();
    assert_eq!(r.taker_closed_pnl, usd("-165"));
    let p = &h.view(1).positions[0];
    assert_eq!(p.szi, sz("-1.5"));
    assert_eq!(p.entry_px, px("1900"));
    // maker 3 was long 0.5 @2100 and bought 3 more: no pnl, long 3.5 @ (1050+5700)/3.5
    assert_eq!(r.maker_closed_pnl, Usd::ZERO);
    assert_eq!(h.ch.position(&addr(3), 1), sz("3.5"));
    assert_eq!(h.view(3).positions[0].entry_px, Px::from_raw(192_857_142_857));
    // 3 sells 1 @ 2000 to 2: removed basis 6750/3.5 = 1928.571428 (toward zero) => +71.428572
    let r = h.fill(3, 2, false, "2000", "1", 1).unwrap();
    assert_eq!(r.taker_closed_pnl, usd("71.428572"));
    // cash = deposits − Σ notional flows − fees, exactly
    let fees1 = usd("0.7") + usd("0.707") + usd("0.3675") + usd("1.995");
    assert_eq!(h.cash(1), usd("100000") - usd("2000") - usd("2020") + usd("1050") + usd("5700") - fees1);
}

#[test]
fn isolated_margin_allocation_release_and_updates() {
    let mut h = H::new("0");
    h.dep(1, "10000");
    h.dep(2, "100000");
    h.ch.update_leverage(addr(1), 0, false, 10).unwrap();
    h.fill(1, 2, true, "100000", "0.1", 0).unwrap();
    let st = h.ch.account(&addr(1)).unwrap();
    // T = 10000/10 = 1000 moved to the isolated bucket
    assert_eq!(st.positions[0].iso_cash, usd("-9000"));
    assert_eq!(st.cash, usd("8996.5"));
    let v = h.view(1);
    assert_eq!(v.positions[0].margin_used, usd("1000"));
    assert_eq!(v.positions[0].leverage, Leverage::Isolated(10));
    assert_eq!(v.withdrawable, usd("8996.5"));
    assert_eq!(v.margin_summary.account_value, usd("9996.5"));
    // isolated liq px: (E − MM) = 1000 − 100 = 900 over 0.1*0.99 => 100000 − 9090.90..
    assert_eq!(v.positions[0].liquidation_px.unwrap(), Px::from_raw(10_000_000_000_000 - 909_090_909_090));
    // partial close half at mark: release half of the equity (500)
    h.fill(1, 2, false, "100000", "0.05", 0).unwrap();
    let st = h.ch.account(&addr(1)).unwrap();
    assert_eq!(st.positions[0].iso_cash, usd("-4500"));
    assert_eq!(st.cash, usd("8996.5") + usd("500") - usd("1.75"));
    // add / remove isolated margin
    h.ch.update_isolated_margin(addr(1), 0, usd("100")).unwrap();
    h.inv();
    assert_eq!(h.view(1).positions[0].margin_used, usd("600"));
    // IM of remaining = 5000/10 = 500 → can remove 100, not 101 more
    assert_eq!(h.ch.update_isolated_margin(addr(1), 0, usd("-100.000001")).unwrap_err().0, ERR_ISO_REMOVE);
    h.ch.update_isolated_margin(addr(1), 0, usd("-100")).unwrap();
    h.inv();
    assert_eq!(h.ch.update_isolated_margin(addr(1), 0, usd("1000000")).unwrap_err().0, ERR_ISO_ADD);
    assert_eq!(h.ch.update_isolated_margin(addr(1), 1, usd("1")).unwrap_err().0, ERR_NO_ISO_POSITION);
    // full close releases everything to cross
    h.fill(1, 2, false, "100000", "0.05", 0).unwrap();
    let st = h.ch.account(&addr(1)).unwrap();
    assert!(st.positions.is_empty());
    assert_eq!(st.cash, usd("10000") - usd("3.5") - usd("1.75") - usd("1.75"));
}

#[test]
fn leverage_rules() {
    let mut h = H::new("0");
    h.dep(1, "1000");
    h.dep(2, "100000");
    assert_eq!(h.ch.leverage_of(&addr(1), 0), Leverage::Cross(20));
    assert_eq!(h.ch.leverage_of(&addr(1), 2), Leverage::Isolated(10)); // min(20, 10), only isolated
    assert_eq!(h.ch.update_leverage(addr(1), 0, true, 51).unwrap_err().0, ERR_LEVERAGE);
    assert_eq!(h.ch.update_leverage(addr(1), 0, true, 0).unwrap_err().0, ERR_LEVERAGE);
    assert_eq!(h.ch.update_leverage(addr(1), 2, true, 5).unwrap_err().0, ERR_CROSS_NOT_ALLOWED);
    h.ch.update_leverage(addr(1), 0, true, 50).unwrap();
    // 0.4 BTC = 40000 notional at 50x => IM 800
    h.fill(1, 2, true, "100000", "0.4", 0).unwrap();
    assert_eq!(h.ch.update_leverage(addr(1), 0, false, 50).unwrap_err().0, ERR_SWITCH_MODE);
    // decreasing to 20x needs IM 2000 > equity ~986
    assert_eq!(h.ch.update_leverage(addr(1), 0, true, 20).unwrap_err().0, ERR_LEVERAGE_MARGIN);
    h.ch.update_leverage(addr(1), 0, true, 45).unwrap(); // IM 889 ok
    h.ch.update_leverage(addr(1), 0, true, 50).unwrap(); // increasing always ok
    // isolated: decrease requires isolated equity
    h.ch.update_leverage(addr(1), 1, false, 10).unwrap();
    h.fill(1, 2, true, "2000", "0.5", 1).unwrap(); // iso margin 100 (cross available 186)
    assert!(h.fill(1, 2, true, "2000", "1", 1).is_err()); // would need 200 more
    assert_eq!(h.ch.update_leverage(addr(1), 1, false, 5).unwrap_err().0, ERR_LEVERAGE_MARGIN);
    h.ch.update_leverage(addr(1), 1, false, 20).unwrap();
    h.inv();
}

#[test]
fn transfers_withdrawals_and_spot() {
    let mut h = H::new("0");
    h.dep(1, "1000");
    h.dep(2, "100000");
    h.fill(1, 2, true, "100000", "0.1", 0).unwrap(); // IM 500, equity 996.5
    assert_eq!(h.ch.withdrawable(&addr(1)), usd("496.5"));
    assert_eq!(h.ch.withdraw(addr(1), usd("496.6")).unwrap_err().0, ERR_WITHDRAW);
    h.ch.withdraw(addr(1), usd("100")).unwrap();
    h.flows.withdrawals += usd("100");
    h.inv();
    // withdraw3-style with fee: 50 debited, 1 to fee pool, 49 leaves
    h.ch.withdraw_with_fee(addr(1), usd("50"), usd("1")).unwrap();
    h.flows.withdrawals += usd("49");
    h.inv();
    assert_eq!(h.ch.usd_send(addr(1), addr(3), usd("400")).unwrap_err().0, ERR_TRANSFER);
    h.ch.usd_send(addr(1), addr(3), usd("300")).unwrap();
    h.inv();
    assert_eq!(h.cash(3), usd("300"));
    // class transfers
    h.ch.class_transfer(addr(3), usd("200"), false).unwrap();
    assert_eq!(h.ch.spot_balance(&addr(3), 0), usd("200"));
    assert_eq!(h.ch.class_transfer(addr(3), usd("201"), true).unwrap_err().0, ERR_TRANSFER);
    h.ch.class_transfer(addr(3), usd("150"), true).unwrap();
    h.inv();
    assert_eq!(h.cash(3), usd("250"));
    // spot sends (PURR, token 1; USDC token 0)
    h.ch.spot_send(addr(9), addr(4), 1, usd("10")).unwrap();
    assert_eq!(h.ch.spot_send(addr(9), addr(4), 1, usd("991")).unwrap_err().0, ERR_SPOT);
    assert_eq!(h.ch.spot_send(addr(9), addr(4), 7, usd("1")).unwrap_err().0, ERR_TOKEN);
    h.ch.spot_send(addr(9), addr(4), 0, usd("500")).unwrap();
    h.inv();
    assert_eq!(h.view(4).spot_balances, vec![(0, usd("500")), (1, usd("10"))]);
    assert_eq!(h.view(9).spot_balances, vec![(1, usd("990"))]);
    assert!(h.ch.deposit(INSURANCE, usd("1")).is_err());
    assert!(h.ch.deposit(addr(1), usd("0")).is_err());
}

#[test]
fn funding_interest_only_and_premium() {
    let mut h = H::new("0");
    h.dep(1, "100000");
    h.dep(2, "100000");
    h.fill(1, 2, true, "100000", "1", 0).unwrap();
    // samples at mark == oracle => premium 0 => rate = interest 0.00125%
    let mut t = T0;
    for _ in 0..720 {
        t += 5_000;
        let ev = h.ch.on_block_funding(t);
        if !ev.is_empty() {
            assert_eq!(t / 3_600_000, T0 / 3_600_000 + 1);
            break;
        }
    }
    let m = h.ch.market_state(0);
    assert_eq!(m.last_funding_rate, Rate::from_raw(12_500_000));
    // 1 BTC × 100000 × 0.0000125 = 1.25
    let v1 = h.view(1);
    assert_eq!(v1.positions[0].cum_funding_since_open, usd("1.25"));
    assert_eq!(h.view(2).positions[0].cum_funding_since_open, usd("-1.25"));
    assert_eq!(h.cash(1), usd("100000") - usd("100000") - usd("35") - usd("1.25"));
    h.inv();
    // premium: mark 1% above oracle => rate = 0.01 + clamp(0.0000125 − 0.01) = 0.0095
    let t1 = t;
    // the settlement block already took the new hour's first sample (premium 0)
    assert_eq!(h.ch.market_state(0).premium_samples, 1);
    h.ch.set_prices(0, px("100000"), px("101000"), None, t);
    let mut ev = vec![];
    while ev.is_empty() {
        t += 1_000;
        ev = h.ch.on_block_funding(t);
    }
    assert_eq!(t / 3_600_000, t1 / 3_600_000 + 1);
    // samples at most once per 5 s: 1 zero sample + (n-1) samples of 1%
    let n = ((t - 1_000) / 5_000 - t1 / 5_000 + 1) as i128;
    let avg = (n - 1) * 10_000_000_000 / n;
    assert_eq!(h.ch.market_state(0).last_funding_rate, Rate::from_raw(avg - 500_000_000));
    let paid = 100_000 * avg - 100_000 * 500_000_000; // 1 BTC × 1e5 × rate, in 1e-12 USD
    let paid_micro = (paid + 999_999) / 1_000_000;
    // samples at most once per 5s
    assert!(ev.len() == 2);
    match &ev[0] {
        Event::Funding { user, usdc, .. } => {
            assert_eq!(*user, addr(1));
            assert_eq!(*usdc, Usd::from_raw(-paid_micro));
        }
        e => panic!("{e:?}"),
    }
    h.inv();
}

#[test]
fn funding_dust_goes_to_insurance() {
    let mut h = H::new("0");
    for a in 1..=4 {
        h.dep(a, "1000");
    }
    // three longs of 1e-8 BTC vs one short of 3e-8
    for a in 1..=3 {
        h.ch
            .apply_fill(&FillParams {
                asset: 0,
                px: px("100000"),
                sz: Sz::from_raw(1),
                taker: addr(a),
                maker: addr(4),
                taker_side: Side::Bid,
                liquidation_of: None,
                time: T0,
            })
            .unwrap();
    }
    let ev = h.ch.on_block_funding(T0 + 3_600_000);
    // each long owes 1e-8*1e5*1.25e-5 = 1.25e-8 USD -> pays 1e-6 (ceil); short receives floor(3.75e-8) = 0
    let pays: Vec<Usd> = ev
        .iter()
        .filter_map(|e| if let Event::Funding { usdc, .. } = e { Some(*usdc) } else { None })
        .collect();
    assert_eq!(pays, vec![Usd::from_raw(-1), Usd::from_raw(-1), Usd::from_raw(-1), Usd::ZERO]);
    assert_eq!(h.ch.ledger().funding_dust_total, Usd::from_raw(3));
    assert_eq!(h.ch.insurance_fund(), Usd::from_raw(3));
    h.inv();
}

fn eth_long_setup(h: &mut H, deposit: &str) {
    h.dep(1, deposit);
    h.dep(2, "100000");
    h.fill(1, 2, true, "2000", "1", 1).unwrap();
}

#[test]
fn liquidation_detection_and_targets() {
    let mut h = H::new("1000000");
    eth_long_setup(&mut h, "1000");
    // equity = 1000 − 0.7 + (p − 2000); MM = p/50
    h.price(1, "1030"); // 29.3 vs 20.6
    assert!(h.ch.liquidation_targets().is_empty());
    h.price(1, "1020"); // 19.3 vs 20.4
    let t = h.ch.liquidation_targets();
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].user, addr(1));
    assert_eq!(t[0].isolated_asset, None);
    assert_eq!(t[0].closes, vec![(1, sz("-1"), px("999.6"))]);
    assert_eq!(t, h.ch.liquidation_targets_full_scan());
    // liquidation fill through the book skips the liquidated side's margin check
    h.dep(3, "100000");
    let r = h
        .ch
        .apply_fill(&FillParams {
            asset: 1,
            px: px("1010"),
            sz: sz("0.1"),
            taker: addr(1),
            maker: addr(3),
            taker_side: Side::Ask,
            liquidation_of: Some(addr(1)),
            time: T0,
        })
        .unwrap();
    assert_eq!(r.taker_closed_pnl, usd("-99"));
    h.inv();
    // remainder via insurance backstop at mark (equity positive: no bad debt)
    let ev = h.ch.backstop_and_adl(addr(1), T0);
    h.inv();
    assert!(ev.iter().any(|e| matches!(e, Event::Fill(f) if f.liquidation.as_ref().unwrap().method == LiquidationMethod::Backstop)));
    assert!(!ev.iter().any(|e| matches!(e, Event::BadDebtCovered { .. })));
    assert_eq!(h.ch.position(&addr(1), 1), Sz::ZERO);
    assert_eq!(h.ch.position(&INSURANCE, 1), sz("0.9"));
    // user keeps exactly its equity: 1000 − 0.7 − 2000 + 101 − 0.03535 (fee) + 918
    assert_eq!(h.cash(1), usd("18.26465"));
    assert!(h.ch.liquidation_targets().is_empty());
    assert!(h.ch.negative_equity_accounts().is_empty());
}

#[test]
fn bad_debt_covered_by_insurance() {
    let mut h = H::new("1000000");
    eth_long_setup(&mut h, "1000");
    h.price(1, "900"); // equity 1000 − 0.7 − 1100 = −100.7
    assert_eq!(h.ch.liquidation_targets().len(), 1);
    let ev = h.ch.backstop_and_adl(addr(1), T0);
    h.inv();
    assert!(ev.contains(&Event::BadDebtCovered { asset: 1, user: addr(1), amount: usd("100.7"), by_insurance: true }));
    assert_eq!(h.cash(1), Usd::ZERO);
    assert_eq!(h.ch.position(&INSURANCE, 1), sz("1"));
    assert_eq!(h.ch.insurance_fund(), usd("1000000") - usd("100.7"));
    assert!(h.ch.negative_equity_accounts().is_empty());
}

#[test]
fn adl_when_insurance_exhausted() {
    let mut h = H::new("0");
    h.dep(1, "101");
    h.dep(2, "1000");
    h.dep(3, "1000");
    h.fill(1, 2, true, "2000", "0.5", 1).unwrap();
    h.fill(1, 3, true, "2000", "0.5", 1).unwrap();
    // 2 opened its short at 2000; 3 too. Make 3 more profitable by leverage: 3 has same entry.
    h.ch.update_leverage(addr(3), 1, true, 25).unwrap();
    h.price(1, "1800"); // equity of 1 = 101 − 0.7 − 200 = −99.7
    let ev = h.ch.backstop_and_adl(addr(1), T0);
    h.inv();
    // ranking: both uPnL 100 on 0.5; score = upnl/margin*lev: margin = 900/lev → score ∝ lev²: 3 first
    let adls: Vec<_> = ev.iter().filter_map(|e| if let Event::Adl { counterparty, sz, .. } = e { Some((*counterparty, *sz)) } else { None }).collect();
    assert_eq!(adls, vec![(addr(3), sz("0.5")), (addr(2), sz("0.5"))]);
    assert!(ev.contains(&Event::BadDebtCovered { asset: 1, user: addr(1), amount: usd("99.7"), by_insurance: false }));
    assert_eq!(h.cash(1), Usd::ZERO);
    assert_eq!(h.ch.position(&addr(2), 1), Sz::ZERO);
    assert_eq!(h.ch.position(&addr(3), 1), Sz::ZERO);
    // counterparties closed at bankruptcy px 1899.4: pnl = 1000 − 949.7 = 50.15 each (split 1899.7 total)
    let total = h.cash(2) + h.cash(3);
    assert_eq!(total, usd("2000") - usd("0.1") - usd("0.1") + usd("2000") - usd("1899.7"));
    assert!(h.ch.negative_equity_accounts().is_empty());
    assert_eq!(h.ch.ledger().bad_debt_adl, usd("99.7"));
}

#[test]
fn isolated_liquidation_deficit_covered() {
    let mut h = H::new("50");
    h.dep(1, "1000");
    h.dep(2, "100000");
    h.dep(3, "100000");
    h.ch.update_leverage(addr(1), 1, false, 10).unwrap();
    h.fill(1, 2, true, "2000", "1", 1).unwrap(); // iso margin 200
    let cross_before = h.cash(1);
    h.price(1, "1700"); // iso equity −100 < MM 34
    let t = h.ch.liquidation_targets();
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].isolated_asset, Some(1));
    // market liquidation below bankruptcy: deficit parked, cross untouched
    h.ch
        .apply_fill(&FillParams {
            asset: 1,
            px: px("1700"),
            sz: sz("1"),
            taker: addr(1),
            maker: addr(3),
            taker_side: Side::Ask,
            liquidation_of: Some(addr(1)),
            time: T0,
        })
        .unwrap();
    h.inv();
    let fee = usd("0.595");
    assert_eq!(h.cash(1), cross_before - fee);
    assert_eq!(h.ch.account(&addr(1)).unwrap().iso_deficit, usd("-100"));
    assert!(!h.ch.negative_equity_accounts().is_empty());
    let ev = h.ch.backstop_and_adl(addr(1), T0);
    h.inv();
    // insurance (50) covers all of it even if that takes it negative (no positions left to ADL)
    assert!(ev.contains(&Event::BadDebtCovered { asset: 0, user: addr(1), amount: usd("100"), by_insurance: true }));
    assert!(h.ch.negative_equity_accounts().is_empty());
    assert_eq!(h.ch.insurance_fund(), usd("-50"));
}

#[test]
fn dirty_tracking_and_snapshot_roundtrip() {
    let mut h = H::new("10");
    let d0 = h.ch.take_dirty();
    assert!(d0.iter().any(|(k, _)| *k == StateKey::Insurance));
    assert!(h.ch.take_dirty().is_empty());
    h.dep(1, "1000");
    h.dep(2, "1000");
    h.fill(1, 2, true, "2000", "0.1", 1).unwrap();
    let d = h.ch.take_dirty();
    let keys: Vec<StateKey> = d.iter().map(|x| x.0).collect();
    assert_eq!(keys, vec![StateKey::Account(addr(1)), StateKey::Account(addr(2)), StateKey::Market(1), StateKey::Insurance]);
    let bytes = h.ch.encode();
    let mut c2 = Clearinghouse::decode(&bytes).unwrap();
    assert_eq!(c2.encode(), bytes);
    assert_eq!(c2.all_entity_hashes(), h.ch.all_entity_hashes());
    c2.check_invariants(&h.flows, 0).unwrap();
    assert_eq!(c2.account_view(&addr(1)), h.ch.account_view(&addr(1)));
    // same op on both → same hashes
    h.price(1, "1500");
    c2.set_prices(1, px("1500"), px("1500"), None, T0);
    assert_eq!(c2.liquidation_targets(), h.ch.liquidation_targets());
    assert_eq!(c2.all_entity_hashes(), h.ch.all_entity_hashes());
    assert!(Clearinghouse::decode(&bytes[..10]).is_err());
}

#[test]
fn liquidation_index_flags_moved_accounts() {
    let mut h = H::new("1000000");
    for a in 1..=20u8 {
        h.dep(a, "1000");
    }
    h.dep(100, "10000000");
    for a in 1..=20u8 {
        // increasing leverage → increasingly close liquidation prices
        h.ch.update_leverage(addr(a), 1, true, a.min(25) as u32 + 2).unwrap();
        let s = Sz::from_raw((a as i128 + 2) * 40_000_000); // notional ≈ (a+2)*800
        h.ch
            .apply_fill(&FillParams { asset: 1, px: px("2000"), sz: s, taker: addr(a), maker: addr(100), taker_side: Side::Bid, liquidation_of: None, time: T0 })
            .unwrap();
    }
    h.price(1, "2000");
    assert!(h.ch.stale.is_empty());
    // a 1% move: only accounts whose bands are left get re-evaluated
    h.ch.set_prices(1, px("1980"), px("1980"), None, T0);
    let mut p = 1980i64;
    while p > 1000 {
        p -= 7;
        h.ch.set_prices(1, Px::from_raw(p as i128 * 100_000_000), Px::from_raw(p as i128 * 100_000_000), None, T0);
        assert_eq!(h.ch.liquidation_targets(), h.ch.liquidation_targets_full_scan(), "at px {p}");
    }
}
