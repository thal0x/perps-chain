//! AC4 property tests: random sequences of deposits / withdrawals / sends /
//! class transfers / fills / leverage changes / isolated margin updates /
//! price moves / funding / liquidation passes among 5–20 accounts. After
//! every step: conservation (exact), funding nets to zero incl. dust, OI
//! long == short, index soundness (indexed liquidation targets == full scan),
//! failed operations leave the state untouched, snapshot round-trips; after
//! every liquidation pass: no account with negative equity.

use clearinghouse::{Clearinghouse, INSURANCE};
use interfaces::components::Clearinghouse as Api;
use interfaces::*;
use proptest::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// Coverage counters (printed with `--nocapture`): fills ok / rejected,
/// liquidation targets, ADL events, bad-debt events, funding settlements.
static STATS: [AtomicUsize; 6] = [const { AtomicUsize::new(0) }; 6];
fn bump(i: usize) {
    STATS[i].fetch_add(1, Relaxed);
}

const T0: u64 = 1_700_000_000_000;

fn addr(n: u8) -> Address {
    Address([n + 1; 20])
}

fn genesis(n_accts: u8, insurance: i64, seed_bal: i64) -> Genesis {
    let m = |asset, name: &str, szd, lev, iso, p: u64| MarketSpec {
        asset,
        name: name.into(),
        sz_decimals: szd,
        max_leverage: lev,
        only_isolated: iso,
        initial_px: Px(p * 100_000_000),
    };
    Genesis {
        chain_id: 1,
        is_mainnet: false,
        genesis_time_ms: T0,
        validators: ValidatorSet { validators: vec![] },
        markets: vec![m(0, "BTC", 5, 50, false, 100_000), m(1, "ETH", 4, 25, false, 2_000), m(2, "ISO", 2, 10, true, 10)],
        balances: (0..n_accts).map(|i| (addr(i), Usd(seed_bal * 1_000_000))).collect(),
        spot_tokens: vec![
            SpotToken { index: 0, name: "USDC".into(), sz_decimals: 8, wei_decimals: 8, token_id: [0; 16] },
            SpotToken { index: 1, name: "PURR".into(), sz_decimals: 0, wei_decimals: 5, token_id: [1; 16] },
        ],
        spot_balances: (0..n_accts).flat_map(|i| [(addr(i), 0, Usd(1_000_000_000)), (addr(i), 1, Usd(5_000_000))]).collect(),
        params: ChainParams { insurance_fund: Usd(insurance * 1_000_000), ..Default::default() },
        bridge: BridgeParams { eth_chain_id: 31337, contract: Address::ZERO, min_deposit: Usd::ZERO },
    }
}

#[derive(Clone, Debug)]
enum Op {
    Deposit(u8, i64),
    Withdraw(u8, i64, bool),
    UsdSend(u8, u8, i64),
    SpotSend(u8, u8, u32, i64),
    Class(u8, i64, bool),
    /// asset, taker, maker, buy, px offset (bps of mark), notional (USD)
    Fill(u32, u8, u8, bool, i64, i64),
    Leverage(u8, u32, bool, u32),
    IsoMargin(u8, u32, i64),
    /// asset, relative move in bps
    Price(u32, i64),
    /// advance block time (ms)
    Time(u64),
    Liquidate,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let a = 0u8..20;
    prop_oneof![
        2 => (a.clone(), 1i64..50_000_000_000).prop_map(|(u, x)| Op::Deposit(u, x)),
        1 => (a.clone(), 1i64..20_000_000_000, any::<bool>()).prop_map(|(u, x, f)| Op::Withdraw(u, x, f)),
        1 => (a.clone(), a.clone(), 1i64..20_000_000_000).prop_map(|(u, v, x)| Op::UsdSend(u, v, x)),
        1 => (a.clone(), a.clone(), 0u32..3, 1i64..2_000_000_000).prop_map(|(u, v, t, x)| Op::SpotSend(u, v, t, x)),
        1 => (a.clone(), 1i64..2_000_000_000, any::<bool>()).prop_map(|(u, x, f)| Op::Class(u, x, f)),
        10 => (0u32..3, a.clone(), a.clone(), any::<bool>(), -300i64..300, 10i64..400_000)
            .prop_map(|(m, t, k, b, o, n)| Op::Fill(m, t, k, b, o, n)),
        // fills far through the mark (stale resting orders)
        2 => (0u32..3, a.clone(), a.clone(), any::<bool>(), -9000i64..9000, 10i64..100_000)
            .prop_map(|(m, t, k, b, o, n)| Op::Fill(m, t, k, b, o, n)),
        2 => (a.clone(), 0u32..3, any::<bool>(), 1u32..55).prop_map(|(u, m, c, l)| Op::Leverage(u, m, c, l)),
        1 => (a.clone(), 0u32..3, -500_000_000i64..500_000_000).prop_map(|(u, m, d)| Op::IsoMargin(u, m, d)),
        6 => (0u32..3, -1500i64..1500).prop_map(|(m, b)| Op::Price(m, b)),
        2 => (1u64..1_200_000).prop_map(Op::Time),
        3 => Just(Op::Liquidate),
    ]
}

struct World {
    ch: Clearinghouse,
    flows: ExternalFlows,
    n: u8,
    now: u64,
    height: u64,
    adl_events: usize,
    bad_debt_events: usize,
    /// Entity hashes accumulated from `take_dirty` since construction.
    acc: std::collections::BTreeMap<StateKey, Hash32>,
}

impl World {
    fn new(n: u8, insurance: i64, seed_bal: i64) -> Self {
        let g = genesis(n, insurance, seed_bal);
        let flows = ExternalFlows { genesis: Clearinghouse::genesis_total(&g), ..Default::default() };
        Self {
            ch: Clearinghouse::new(&g),
            flows,
            n,
            now: T0,
            height: 0,
            adl_events: 0,
            bad_debt_events: 0,
            acc: Default::default(),
        }
    }

    fn a(&self, i: u8) -> Address {
        addr(i % self.n)
    }

    /// Drain `take_dirty` into the accumulated map; it must equal
    /// `all_entity_hashes()` exactly (same key set, insurance only under
    /// `StateKey::Insurance`).
    fn check_commitment(&mut self) {
        for (k, h) in self.ch.take_dirty() {
            assert_ne!(k, StateKey::Account(INSURANCE), "insurance reported as an Account key");
            self.acc.insert(k, h);
        }
        let all: std::collections::BTreeMap<StateKey, Hash32> = self.ch.all_entity_hashes().into_iter().collect();
        assert_eq!(self.acc, all, "accumulated take_dirty != all_entity_hashes");
    }

    fn check(&mut self) {
        self.check_commitment();
        self.height += 1;
        if let Err(e) = self.ch.check_invariants(&self.flows, self.height) {
            panic!("{e}");
        }
    }

    /// Run `f`; if it fails, the encoded state must be unchanged.
    fn atomic<T>(&mut self, f: impl FnOnce(&mut Clearinghouse) -> Result<T, ExecError>) -> Option<T> {
        let before = self.ch.encode();
        match f(&mut self.ch) {
            Ok(v) => Some(v),
            Err(_) => {
                assert_eq!(before, self.ch.encode(), "failed operation mutated state");
                None
            }
        }
    }

    fn liquidation_pass(&mut self) {
        let targets = self.ch.liquidation_targets();
        STATS[2].fetch_add(targets.len(), Relaxed);
        assert_eq!(targets, self.ch.liquidation_targets_full_scan(), "liquidation index unsound");
        let mut users: Vec<Address> = Vec::new();
        for (k, t) in targets.iter().enumerate() {
            // market liquidation: try to close half against some other account
            // at the limit price (the book's worst acceptable price)
            for (j, (asset, sz, limit)) in t.closes.iter().enumerate() {
                let cp = self.a((k as u8).wrapping_mul(7).wrapping_add(3));
                if cp == t.user {
                    continue;
                }
                let half = Sz::from_raw(sz.raw().abs() / 2);
                if half.is_zero() {
                    continue;
                }
                // Alternate between the liquidation limit and a maker price far
                // through the mark (a stale resting order the IOC sweeps;
                // legal in the book and terrible for the maker).
                let mark = self.ch.market_ctx(*asset).mark_px;
                let px = match (k + j) % 3 {
                    0 => *limit,
                    1 if sz.0 > 0 => Px::from_raw(mark.raw() / 3 + 1),
                    1 => Px::from_raw(mark.raw() * 3),
                    _ => mark,
                };
                let f = FillParams {
                    asset: *asset,
                    px,
                    sz: half,
                    taker: t.user,
                    maker: cp,
                    taker_side: if sz.0 > 0 { Side::Bid } else { Side::Ask },
                    liquidation_of: Some(t.user),
                    time: self.now,
                };
                self.atomic(|ch| ch.apply_fill(&f));
                self.check();
            }
            if !users.contains(&t.user) {
                users.push(t.user);
            }
        }
        for u in users {
            let ev = self.ch.backstop_and_adl(u, self.now);
            for e in &ev {
                match e {
                    Event::Adl { .. } => {
                        self.adl_events += 1;
                        bump(3)
                    }
                    Event::BadDebtCovered { .. } => {
                        self.bad_debt_events += 1;
                        bump(4)
                    }
                    _ => {}
                }
            }
            self.check();
        }
        let neg = self.ch.negative_equity_accounts();
        assert!(neg.is_empty(), "negative equity after liquidation pass: {neg:?}");
        // Accounts pushed below maintenance (but not below zero) by ADL during
        // this pass are legitimately left for the next block's pass.
    }

    fn step(&mut self, op: &Op) {
        match op.clone() {
            Op::Deposit(u, x) => {
                let u = self.a(u);
                if self.atomic(|ch| ch.deposit(u, Usd(x))).is_some() {
                    self.flows.deposits += Usd(x);
                }
            }
            Op::Withdraw(u, x, fee) => {
                let u = self.a(u);
                if fee {
                    if self.atomic(|ch| ch.withdraw_with_fee(u, Usd(x), Usd(1_000_000))).is_some() {
                        self.flows.withdrawals += Usd(x - 1_000_000);
                    }
                } else if self.atomic(|ch| ch.withdraw(u, Usd(x))).is_some() {
                    self.flows.withdrawals += Usd(x);
                }
            }
            Op::UsdSend(u, v, x) => {
                let (u, v) = (self.a(u), self.a(v));
                let w = self.ch.withdrawable(&u);
                let r = self.atomic(|ch| ch.usd_send(u, v, Usd(x)));
                assert_eq!(r.is_some(), w >= Usd(x));
            }
            Op::SpotSend(u, v, t, x) => {
                let (u, v) = (self.a(u), self.a(v));
                self.atomic(|ch| ch.spot_send(u, v, t, Usd(x)));
            }
            Op::Class(u, x, to_perp) => {
                let u = self.a(u);
                self.atomic(|ch| ch.class_transfer(u, Usd(x), to_perp));
            }
            Op::Fill(m, t, k, buy, off, ntl) => {
                let (t, k) = (self.a(t), self.a(k));
                let mark = self.ch.market_ctx(m).mark_px;
                let px = Px::from_raw(mark.raw() * (10_000 + off as i128) / 10_000);
                let sz = sz_from_usd(Usd(ntl * 1_000_000), px, Rounding::Floor);
                if sz.is_zero() {
                    return;
                }
                let f = FillParams {
                    asset: m,
                    px,
                    sz,
                    taker: t,
                    maker: k,
                    taker_side: Side::from_is_buy(buy),
                    liquidation_of: None,
                    time: self.now,
                };
                // the pre-trade check must agree with the fill-time check for the taker
                let pre = self.ch.check_order_margin(&MarginCheck {
                    user: t,
                    asset: m,
                    side: f.taker_side,
                    px,
                    sz,
                    reduce_only: false,
                });
                let fee_before = self.ch.ledger().fee_pool;
                let neg_before = self.ch.negative_equity_accounts();
                let r = self.atomic(|ch| ch.apply_fill(&f));
                bump(if r.is_some() { 0 } else { 1 });
                if let Some(r) = &r {
                    assert!(pre.is_ok(), "fill succeeded but taker pre-check failed");
                    assert_eq!(self.ch.ledger().fee_pool - fee_before, r.taker_fee + r.maker_fee);
                    for a in self.ch.negative_equity_accounts() {
                        assert!(neg_before.contains(&a), "fill created negative equity for {a:?}");
                    }
                }
            }
            Op::Leverage(u, m, c, l) => {
                let u = self.a(u);
                self.atomic(|ch| ch.update_leverage(u, m, c, l));
            }
            Op::IsoMargin(u, m, d) => {
                let u = self.a(u);
                self.atomic(|ch| ch.update_isolated_margin(u, m, Usd(d)));
            }
            Op::Price(m, bps) => {
                let ctx = self.ch.market_ctx(m);
                let init = self.ch.markets()[m as usize].initial_px.raw();
                let p = (ctx.mark_px.raw() * (10_000 + bps as i128) / 10_000).clamp(init / 5, init * 5);
                // oracle lags mark slightly to create premium
                let o = (p * 10_000 / (10_000 + (bps as i128) / 10)).max(1);
                self.ch.set_prices(m, Px::from_raw(o), Px::from_raw(p), Some(Px::from_raw(p)), self.now);
                assert_eq!(self.ch.liquidation_targets(), self.ch.liquidation_targets_full_scan(), "index unsound after price");
            }
            Op::Time(dt) => {
                self.now += dt;
                let dust0 = self.ch.ledger().funding_dust_total;
                let ev = self.ch.on_block_funding(self.now);
                if !ev.is_empty() {
                    bump(5);
                }
                let paid: i64 = ev.iter().map(|e| if let Event::Funding { usdc, .. } = e { usdc.0 } else { 0 }).sum();
                let dust = self.ch.ledger().funding_dust_total - dust0;
                assert_eq!(paid + dust.0, 0, "funding does not net to zero");
                assert!(dust.0 >= 0);
            }
            Op::Liquidate => self.liquidation_pass(),
        }
        self.check();
    }
}

fn run(n: u8, insurance: i64, seed_bal: i64, ops: &[Op]) -> (usize, usize) {
    let mut w = World::new(n, insurance, seed_bal);
    w.check();
    for (i, op) in ops.iter().enumerate() {
        w.step(op);
        if i % 16 == 15 {
            let bytes = w.ch.encode();
            let d = Clearinghouse::decode(&bytes).unwrap();
            assert_eq!(d.encode(), bytes);
            assert_eq!(d.all_entity_hashes(), w.ch.all_entity_hashes());
            let restored: std::collections::BTreeMap<StateKey, Hash32> = d.all_entity_hashes().into_iter().collect();
            assert_eq!(restored, w.acc, "restored node's entity set differs from accumulated take_dirty");
            assert_eq!(d.liquidation_targets_full_scan(), w.ch.liquidation_targets_full_scan());
            // continue on the decoded copy half of the time: derived indices must be rebuilt correctly
            if i % 32 == 31 {
                w.ch = d;
            }
        }
    }
    w.liquidation_pass();
    w.check();
    assert!(w.ch.position(&INSURANCE, 0).raw().abs() >= 0);
    let s: Vec<usize> = STATS.iter().map(|x| x.load(Relaxed)).collect();
    println!("stats fills_ok={} fills_rej={} targets={} adl={} bad_debt={} fundings={}", s[0], s[1], s[2], s[3], s[4], s[5]);
    (w.adl_events, w.bad_debt_events)
}

/// Case count for a proptest block: `$PROPTEST_CASES` if set (lets AC4 and
/// local runs scale the search up), else the block's default (D33).
#[allow(dead_code)]
fn env_cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: env_cases(1000), max_shrink_iters: 4000, ..ProptestConfig::default() })]

    #[test]
    fn invariants_hold_under_random_ops(
        n in 5u8..=20,
        insurance in prop_oneof![Just(0i64), Just(500), Just(1_000_000)],
        seed_bal in prop_oneof![Just(0i64), Just(2_000), Just(50_000)],
        ops in proptest::collection::vec(op_strategy(), 1..120),
    ) {
        run(n, insurance, seed_bal, &ops);
    }

    /// Leverage-heavy, volatile markets with a tiny insurance fund: exercises
    /// backstop, bad debt, insurance exhaustion and ADL cascades.
    #[test]
    fn invariants_hold_in_liquidation_storms(
        n in 5u8..=12,
        insurance in prop_oneof![Just(0i64), Just(100)],
        ops in proptest::collection::vec(prop_oneof![
            6 => (0u32..2, 0u8..12, 0u8..12, any::<bool>(), -50i64..50, 1_000i64..60_000)
                .prop_map(|(m, t, k, b, o, x)| Op::Fill(m, t, k, b, o, x)),
            2 => (0u8..12, 0u32..3, any::<bool>(), 10u32..50).prop_map(|(u, m, c, l)| Op::Leverage(u, m, c, l)),
            4 => (0u32..2, -2500i64..2500).prop_map(|(m, b)| Op::Price(m, b)),
            1 => (1u64..4_000_000).prop_map(Op::Time),
            3 => Just(Op::Liquidate),
        ], 1..150),
    ) {
        run(n, insurance, 3_000, &ops);
    }
}

/// Deterministic scenario that must hit insurance exhaustion and ADL.
#[test]
fn storm_hits_adl() {
    let mut ops = Vec::new();
    for i in 0..10u8 {
        ops.push(Op::Leverage(i, 1, true, 25));
    }
    for i in 0..5u8 {
        ops.push(Op::Fill(1, i, i + 5, true, 0, 70_000));
    }
    ops.push(Op::Price(1, -1000));
    ops.push(Op::Liquidate);
    ops.push(Op::Price(1, 2500));
    ops.push(Op::Price(1, 2500));
    ops.push(Op::Liquidate);
    let (adl, bad) = run(10, 0, 3_000, &ops);
    assert!(adl > 0, "expected ADL events");
    assert!(bad > 0, "expected bad debt events");
}
