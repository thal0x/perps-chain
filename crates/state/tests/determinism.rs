//! Determinism (same blocks => same roots), snapshot/restore, and randomized
//! multi-account invariant checks (AC2 / AC4 slice of the state crate).
mod common;
use common::*;
use interfaces::Application;
use interfaces::*;
use proptest::prelude::*;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;
use state::{ChainConfig, TestChain};
use std::sync::Arc;

const USERS: u64 = 12;

fn genesis() -> Genesis {
    let bal: Vec<(Address, Usd)> = (1..=USERS).map(|i| (addr(i), usd(if i <= 3 { "2000000" } else { "3000" }))).collect();
    let mut g = test_genesis(4, &bal);
    g.spot_balances = (1..=USERS).map(|i| (addr(i), 1u32, usd("100"))).collect();
    g
}

fn rand_px(rng: &mut ChaCha20Rng, mid: u64, spread_bps: u64, decimals_round: u64) -> Px {
    let off = mid * rng.gen_range(0..=spread_bps) / 10_000;
    let p = if rng.gen_bool(0.5) { mid + off } else { mid - off };
    // round to a multiple that keeps <= 5 sig figs
    Px(p / decimals_round * decimals_round)
}

/// Seeded random block generator: market makers, takers, cancels, modifies,
/// transfers, triggers, oracle moves, deposits.
pub struct Gen {
    rng: ChaCha20Rng,
    pub time: u64,
    nonce: u64,
    vnonce: u64,
    mids: [u64; 2],
    dep_id: u64,
    pub height: u64,
}

impl Gen {
    pub fn new(seed: u64) -> Self {
        Self {
            rng: ChaCha20Rng::seed_from_u64(seed),
            time: GENESIS_TIME,
            nonce: GENESIS_TIME,
            vnonce: 0,
            mids: [60_000 * 100_000_000, 3_000 * 100_000_000],
            dep_id: 0,
            height: 0,
        }
    }
    fn n(&mut self) -> u64 {
        self.nonce = self.nonce.max(self.time) + 1;
        self.nonce
    }
    fn order(&mut self, maker: bool) -> OrderRequest {
        let asset = self.rng.gen_range(0..2u32);
        let mid = self.mids[asset as usize];
        let round = if asset == 0 { 10 * 100_000_000 } else { 100_000_000 / 2 };
        let is_buy = self.rng.gen_bool(0.5);
        let (px, tif) = if maker {
            let p = rand_px(&mut self.rng, mid, 50, round);
            let p = if is_buy { p.0.min(mid - round) } else { p.0.max(mid + round) };
            let p = Px(p / round * round);
            (p, if self.rng.gen_bool(0.2) { Tif::Alo } else { Tif::Gtc })
        } else {
            let p = if is_buy { mid + mid / 50 } else { mid - mid / 50 };
            (Px(p / round * round), if self.rng.gen_bool(0.7) { Tif::Ioc } else { Tif::Gtc })
        };
        let sz_unit: i64 = if asset == 0 { 1_000 } else { 10_000 };
        let s = Sz(sz_unit * if maker { self.rng.gen_range(20..8000) } else { self.rng.gen_range(20..2500) });
        OrderRequest {
            asset,
            is_buy,
            limit_px: px,
            sz: s,
            reduce_only: !maker && self.rng.gen_bool(0.1),
            kind: OrderKind::Limit { tif },
            cloid: if self.rng.gen_bool(0.1) { Some(self.rng.gen()) } else { None },
        }
    }
    pub fn block(&mut self, chain: &TestChain) -> (BlockContext, Vec<Tx>) {
        self.time += self.rng.gen_range(200..1500);
        self.height += 1;
        let mut txs = vec![];
        // random walk mids
        let shock = self.rng.gen_bool(0.03);
        let up = self.rng.gen_bool(0.5);
        for m in self.mids.iter_mut() {
            if shock {
                *m = if up { *m + *m / 12 } else { *m - *m / 12 };
            }
            let step = *m / 300;
            if self.rng.gen_bool(0.5) {
                *m += self.rng.gen_range(0..=step);
            } else {
                *m -= self.rng.gen_range(0..=step);
            }
        }
        if self.rng.gen_bool(0.5) {
            for v in 0..4u64 {
                self.vnonce += 1;
                let prices = (0..2).map(|a| (a as AssetId, Px(self.mids[a] / 100_000_000 * 100_000_000))).collect();
                txs.push(validator_tx(addr(1000 + v), self.vnonce, ValidatorAction::OraclePrices { prices }));
            }
        }
        let n_tx = self.rng.gen_range(0..25);
        for _ in 0..n_tx {
            let u = addr(self.rng.gen_range(1..=USERS));
            let roll = self.rng.gen_range(0..100);
            let action = if roll < 6 && u != addr(1) && u != addr(2) && u != addr(3) {
                // degen: max-leverage taker on BTC sized from account value
                let av = chain.query().account(&u).margin_summary.account_value.0.max(0) as i128;
                let mid = self.mids[0] as i128;
                let lev = chain.clearinghouse().leverage_of(&u, 0).value() as i128;
                let s = (av * lev * 8 / 10 * 100_000_000 * 100 / mid / 1_000 * 1_000) as i64;
                let is_buy = self.rng.gen_bool(0.5);
                let round = 10 * 100_000_000u64;
                let p = if is_buy { self.mids[0] + self.mids[0] / 200 } else { self.mids[0] - self.mids[0] / 200 };
                Action::Order {
                    orders: vec![OrderRequest {
                        asset: 0,
                        is_buy,
                        limit_px: Px(p / round * round),
                        sz: Sz(s.max(1000)),
                        reduce_only: false,
                        kind: OrderKind::Limit { tif: Tif::Ioc },
                        cloid: None,
                    }],
                    grouping: Grouping::Na,
                }
            } else if roll < 35 {
                let k = self.rng.gen_range(1..6);
                Action::Order { orders: (0..k).map(|_| self.order(true)).collect(), grouping: Grouping::Na }
            } else if roll < 60 {
                Action::Order { orders: vec![self.order(false)], grouping: Grouping::Na }
            } else if roll < 75 {
                let open = chain.query().open_orders(&u);
                if open.is_empty() {
                    Action::Noop
                } else {
                    let o = &open[self.rng.gen_range(0..open.len())];
                    if self.rng.gen_bool(0.7) {
                        Action::Cancel { cancels: vec![(o.asset, o.oid)] }
                    } else {
                        let mut r = self.order(true);
                        r.asset = o.asset;
                        r.is_buy = o.side == Side::Bid;
                        r.cloid = None;
                        Action::Modify { oid: OidOrCloid::Oid(o.oid), order: r }
                    }
                }
            } else if roll < 80 {
                let asset = self.rng.gen_range(0..2u32);
                let mid = self.mids[asset as usize];
                let round = if asset == 0 { 10 * 100_000_000 } else { 100_000_000 };
                let is_buy = self.rng.gen_bool(0.5);
                let tpsl = if self.rng.gen_bool(0.5) { TpSl::Tp } else { TpSl::Sl };
                let tp = Px((mid + if is_buy { mid / 200 } else { 0 }) / round * round);
                Action::Order {
                    orders: vec![OrderRequest {
                        asset,
                        is_buy,
                        limit_px: Px((if is_buy { mid + mid / 20 } else { mid - mid / 20 }) / round * round),
                        sz: Sz(if asset == 0 { 100_000 } else { 1_000_000 }),
                        reduce_only: true,
                        kind: OrderKind::Trigger { trigger_px: tp, is_market: true, tpsl },
                        cloid: None,
                    }],
                    grouping: Grouping::Na,
                }
            } else if roll < 85 {
                let n = self.n();
                let d = addr(self.rng.gen_range(1..=USERS + 2));
                Action::UsdSend { destination: d, amount: Usd(self.rng.gen_range(1..200) * 1_000_000), time: n }
            } else if roll < 88 {
                let n = self.n();
                Action::Withdraw3 { destination: addr(99), amount: Usd(self.rng.gen_range(2..50) * 1_000_000), time: n }
            } else if roll < 91 {
                let n = self.n();
                Action::UsdClassTransfer { amount: Usd(self.rng.gen_range(1..50) * 1_000_000), to_perp: self.rng.gen_bool(0.5), nonce: n }
            } else if roll < 93 {
                let n = self.n();
                let tok = format!("PURR:0x{}", hex::encode([0x11u8; 16]));
                Action::SpotSend { destination: addr(self.rng.gen_range(1..=USERS)), token: tok, amount: Usd(1_000_000), time: n }
            } else if roll < 95 {
                Action::UpdateLeverage { asset: self.rng.gen_range(0..2), is_cross: true, leverage: self.rng.gen_range(1..=25) }
            } else if roll < 97 {
                Action::ScheduleCancel { time: Some(self.time + self.rng.gen_range(5_000..30_000)) }
            } else {
                // deposit via 3 validator attestations
                self.dep_id += 1;
                let a = DepositAttestation {
                    deposit_id: self.dep_id,
                    user: u,
                    amount: Usd(self.rng.gen_range(1..1000) * 1_000_000),
                    eth_tx_hash: Hash32::ZERO,
                };
                for v in 0..3u64 {
                    self.vnonce += 1;
                    txs.push(validator_tx(addr(1000 + v), self.vnonce, ValidatorAction::BridgeDeposit(a.clone())));
                }
                continue;
            };
            let nonce = match &action {
                Action::UsdSend { time, .. } | Action::Withdraw3 { time, .. } | Action::SpotSend { time, .. } => *time,
                Action::UsdClassTransfer { nonce, .. } => *nonce,
                _ => self.n(),
            };
            txs.push(user_tx(u, &action, nonce));
        }
        let ctx = BlockContext { height: self.height, timestamp_ms: self.time, proposer: addr(1000), block_id: Hash32::ZERO };
        (ctx, txs)
    }
}

fn new_chain() -> TestChain {
    TestChain::new(genesis(), Arc::new(FakeVerifier), ChainConfig::strict())
}

/// Generate `n` blocks (feeding back from a live chain) and return them with roots.
fn generate(seed: u64, n: usize) -> (Vec<(BlockContext, Vec<Tx>)>, Vec<Hash32>, TestChain) {
    let mut c = new_chain();
    let mut g = Gen::new(seed);
    let mut blocks = vec![];
    let mut roots = vec![];
    for _ in 0..n {
        let (ctx, txs) = g.block(&c);
        let o = c.execute_block(&ctx, &txs);
        roots.push(o.state_root);
        blocks.push((ctx, txs));
    }
    (blocks, roots, c)
}

#[test]
fn same_blocks_same_roots_and_snapshot_restore() {
    let (blocks, roots, c1) = generate(42, 300);
    // Activity sanity: fills, cancels and liquidity happened.
    assert!(c1.clearinghouse().total_collateral().0 != 0);
    // Replay from genesis => identical roots per height.
    let mut c2 = new_chain();
    let mut fills = 0;
    for (i, (ctx, txs)) in blocks.iter().enumerate() {
        let o = c2.execute_block(ctx, txs);
        assert_eq!(o.state_root, roots[i], "root mismatch at height {}", ctx.height);
        fills += o.events.iter().filter(|e| matches!(e, Event::Fill(_))).count();
    }
    assert!(fills > 50, "scenario too quiet: {fills} fills");
    let mut c5 = new_chain();
    let (mut ok, mut errs, mut liq, mut trig) = (0, 0, 0, 0);
    let mut hist = std::collections::BTreeMap::<String, usize>::new();
    for (ctx, txs) in &blocks {
        let o = c5.execute_block(ctx, txs);
        for r in &o.receipts {
            match &r.result {
                Ok(ActionResponse::Order(st)) => {
                    for s in st {
                        if let OrderStatusResp::Error(e) = s { errs += 1; *hist.entry(e.chars().take(40).collect::<String>()).or_insert(0) += 1; } else { ok += 1 }
                    }
                }
                Ok(_) => ok += 1,
                Err(e) => { errs += 1; *hist.entry(e.chars().take(40).collect::<String>()).or_insert(0) += 1; }
            }
        }
        liq += o.events.iter().filter(|e| matches!(e, Event::Fill(f) if f.liquidation.is_some())).count();
        trig += o.events.iter().filter(|e| matches!(e, Event::OrderUpdate { status: OrderLifecycle::Triggered, .. })).count();
    }
    eprintln!("scenario: fills={fills} ok={ok} errors={errs} liquidation_fills={liq} triggered={trig} {hist:?}");
    assert!(liq > 0 && trig > 0, "scenario must exercise liquidations and triggers");
    assert_eq!(c2.root_at(150), Some(roots[149]));
    assert_eq!(c2.root_at(0).is_some(), true);
    // Snapshot at height 150, restore into a fresh chain, continue.
    let mut c3 = new_chain();
    for (ctx, txs) in &blocks[..150] {
        c3.execute_block(ctx, txs);
    }
    let snap = c3.snapshot();
    assert_eq!(snap.height, 150);
    assert_eq!(snap.state_root, roots[149]);
    let mut c4 = new_chain();
    assert_eq!(c4.restore(&snap).unwrap(), roots[149]);
    assert_eq!(c4.executed_height(), 150);
    for (i, (ctx, txs)) in blocks.iter().enumerate().skip(150) {
        let o = c4.execute_block(ctx, txs);
        assert_eq!(o.state_root, roots[i], "post-restore root mismatch at height {}", ctx.height);
    }
    // Corrupted snapshot root is rejected.
    let mut bad = snap.clone();
    bad.state_root = Hash32([1; 32]);
    assert!(new_chain().restore(&bad).is_err());
}

#[test]
fn receipts_and_events_are_deterministic() {
    let (blocks, _, _) = generate(7, 120);
    let run = || {
        let mut c = new_chain();
        blocks.iter().map(|(ctx, txs)| c.execute_block(ctx, txs)).collect::<Vec<_>>()
    };
    assert_eq!(run(), run());
}

#[test]
fn root_changes_with_state() {
    let mut h = H::new(&[(addr(ALICE), "10000")]);
    let r0 = h.chain.current_root();
    h.block(vec![]);
    let r1 = h.chain.current_root();
    assert_ne!(r0, r1, "height/time are committed");
    let n = h.next_nonce();
    let o = h.block(vec![user_tx(addr(ALICE), &Action::Noop, n)]);
    // Same block shape on a twin chain without the noop gives a different root.
    let mut h2 = H::new(&[(addr(ALICE), "10000")]);
    h2.block(vec![]);
    let o2 = h2.block(vec![]);
    assert_ne!(o.state_root, o2.state_root);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: env_cases(32), .. ProptestConfig::default() })]
    /// Invariants (conservation, book uncrossed, OI nets to zero, bridge
    /// accounting) checked after every block of random multi-account flow.
    #[test]
    fn invariants_hold_every_block(seed in any::<u64>()) {
        let mut c = new_chain();
        let mut g = Gen::new(seed);
        for _ in 0..120 {
            let (ctx, txs) = g.block(&c);
            c.execute_block(&ctx, &txs); // strict config panics on violation
            prop_assert!(c.check_invariants().is_ok());
            prop_assert!(c.invariant_violations().is_empty());
        }
    }
}

#[test]
fn restored_entity_set_equals_live() {
    let (blocks, _roots, _) = generate(42, 150);
    let mut c3 = new_chain();
    for (ctx, txs) in &blocks {
        c3.execute_block(ctx, txs);
    }
    let mut c4 = new_chain();
    c4.restore(&c3.snapshot()).unwrap();
    assert_eq!(c3.entity_hashes(), c4.entity_hashes());
}
