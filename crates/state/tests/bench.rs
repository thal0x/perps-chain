//! Execution throughput benchmark (AC3 budget for the state machine).
//!
//! `cargo test --release -p state --test bench -- --ignored --nocapture`
//!
//! Market makers requote with batches of 50 orders per signed action
//! (25 bids + 25 asks) and cancel their previous ladder with one 50-entry
//! cancel action; takers send IOC orders that hit the ladders. Measures
//! `execute_block` wall time on a single rayon thread with the fake verifier
//! (decode is borsh, no ECDSA), i.e. book + clearinghouse + hashing.
mod common;
use common::*;
use interfaces::*;
use state::{ChainConfig, TestChain, InvariantMode};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MMS: u64 = 40;
const TAKERS: u64 = 40;
const BLOCKS: usize = 150;

fn mm_ladder(asset: AssetId, mid: u64, tick: u64, offset: u64, s: Sz) -> Vec<OrderRequest> {
    let mut v = Vec::with_capacity(50);
    for i in 0..25u64 {
        let d = (i + 1 + offset) * tick;
        for (is_buy, p) in [(true, mid - d), (false, mid + d)] {
            v.push(OrderRequest {
                asset,
                is_buy,
                limit_px: Px(p),
                sz: s,
                reduce_only: false,
                kind: OrderKind::Limit { tif: Tif::Gtc },
                cloid: None,
            });
        }
    }
    v
}

#[test]
#[ignore]
fn execution_throughput() {
    let mut bal: Vec<(Address, Usd)> = (1..=MMS).map(|i| (addr(i), usd("100000000"))).collect();
    bal.extend((1..=TAKERS).map(|i| (addr(10_000 + i), usd("10000000"))));
    let g = test_genesis(4, &bal);
    let cfg = ChainConfig { invariants: InvariantMode::Never, ..ChainConfig::default() };
    let mut chain = TestChain::new(g, Arc::new(FakeVerifier), cfg);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    let mut time = GENESIS_TIME;
    let mut nonce = GENESIS_TIME;
    let mut total = Duration::ZERO;
    let mut actions = 0u64;
    let mut fills = 0usize;
    let mut txs_n = 0usize;
    let mut warm_actions = 0u64;
    let mut warm_time = Duration::ZERO;
    for b in 0..BLOCKS {
        time += 500;
        let mut txs = Vec::new();
        for m in 1..=MMS {
            let u = addr(m);
            let asset = (m % 2) as AssetId;
            let (mid, tick, s) = if asset == 0 {
                (60_000 * 100_000_000u64, 100_000_000u64, sz("0.01"))
            } else {
                (3_000 * 100_000_000u64, 10_000_000u64, sz("0.2"))
            };
            // cancel the previous ladder (one 50-entry cancel action)
            let open: Vec<(AssetId, Oid)> = chain.query().open_orders(&u).iter().map(|o| (o.asset, o.oid)).collect();
            if !open.is_empty() {
                nonce += 1;
                txs.push(user_tx(u, &Action::Cancel { cancels: open }, nonce));
            }
            let off = (b as u64 + m) % 3;
            nonce += 1;
            txs.push(user_tx(
                u,
                &Action::Order { orders: mm_ladder(asset, mid, tick, off, s), grouping: Grouping::Na },
                nonce,
            ));
        }
        for t in 1..=TAKERS {
            let asset = (t % 2) as AssetId;
            let is_buy = (t + b as u64) % 2 == 0;
            let (p, s) = if asset == 0 {
                (if is_buy { "60010" } else { "59990" }, "0.02")
            } else {
                (if is_buy { "3001" } else { "2999" }, "0.4")
            };
            nonce += 1;
            txs.push(user_tx(
                addr(10_000 + t),
                &Action::Order { orders: vec![limit(asset, is_buy, p, s, Tif::Ioc)], grouping: Grouping::Na },
                nonce,
            ));
        }
        let ctx = BlockContext { height: b as u64 + 1, timestamp_ms: time, proposer: addr(1000), block_id: Hash32::ZERO };
        txs_n += txs.len();
        let t0 = Instant::now();
        let o = pool.install(|| chain.execute_block(&ctx, &txs));
        let dt = t0.elapsed();
        if b >= 5 {
            warm_actions += o.order_actions;
            warm_time += dt;
        }
        total += dt;
        actions += o.order_actions;
        fills += o.events.iter().filter(|e| matches!(e, Event::Fill(_))).count();
        let errs = o
            .receipts
            .iter()
            .filter(|r| match &r.result {
                Ok(ActionResponse::Order(st)) => st.iter().any(|s| matches!(s, OrderStatusResp::Error(_))),
                Ok(ActionResponse::Cancel(st)) => st.iter().any(|s| matches!(s, SimpleStatus::Error(_))),
                Ok(_) => false,
                Err(_) => true,
            })
            .count();
        assert!(errs <= TAKERS as usize, "unexpected errors in block {b}: {errs}");
    }
    let rate = warm_actions as f64 / warm_time.as_secs_f64();
    println!(
        "blocks={BLOCKS} txs={txs_n} order_actions={actions} fills={fills} total={:?} -> {:.0} order actions/s (steady state)",
        total, rate
    );
    assert!(fills > 1000);
}
