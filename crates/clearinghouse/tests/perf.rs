//! Performance smoke benchmarks (ignored by default; wall-clock timing is
//! test-only). Run with:
//! `cargo test -p clearinghouse --release --test perf -- --ignored --nocapture`

use clearinghouse::Clearinghouse;
use interfaces::components::Clearinghouse as Api;
use interfaces::*;
use std::time::Instant;

const T0: u64 = 1_700_000_000_000;
const MARKETS: u32 = 12;

fn addr(n: u32) -> Address {
    let mut a = [0u8; 20];
    a[..4].copy_from_slice(&n.to_be_bytes());
    a[19] = 1;
    Address(a)
}

fn setup(accounts: u32) -> Clearinghouse {
    let markets = (0..MARKETS)
        .map(|i| MarketSpec {
            asset: i,
            name: format!("M{i}"),
            sz_decimals: 3,
            max_leverage: 20,
            only_isolated: false,
            initial_px: Px((100 + i as u64) * 100_000_000),
        })
        .collect();
    let g = Genesis {
        chain_id: 1,
        is_mainnet: false,
        genesis_time_ms: T0,
        validators: ValidatorSet { validators: vec![] },
        markets,
        balances: (0..accounts).map(|i| (addr(i), Usd(1_000_000 * 1_000_000))).collect(),
        spot_tokens: vec![],
        spot_balances: vec![],
        params: ChainParams::default(),
        bridge: BridgeParams { eth_chain_id: 1, contract: Address::ZERO, min_deposit: Usd::ZERO },
    };
    let mut ch = Clearinghouse::new(&g);
    // every account gets 10 positions (pairs i, i+1 trade opposite sides)
    for i in (0..accounts).step_by(2) {
        for m in 0..10u32 {
            ch.apply_fill(&FillParams {
                asset: m,
                px: Px((100 + m as u64) * 100_000_000),
                sz: Sz(100_000_000 + (i as i64 % 97) * 1_000_000),
                taker: addr(i),
                maker: addr(i + 1),
                taker_side: if (i / 2 + m) % 2 == 0 { Side::Bid } else { Side::Ask },
                liquidation_of: None,
                time: T0,
            })
            .unwrap();
        }
    }
    for m in 0..MARKETS {
        let p = Px((100 + m as u64) * 100_000_000);
        ch.set_prices(m, p, p, None, T0);
    }
    ch.take_dirty();
    ch
}

#[test]
#[ignore]
fn bench_apply_fill_and_margin_check() {
    let n: u32 = std::env::var("CH_BENCH_ACCOUNTS").ok().and_then(|v| v.parse().ok()).unwrap_or(100_000);
    let mut ch = setup(n);
    let iters = 400_000u32;
    let makers: Option<u32> = std::env::var("CH_BENCH_MAKERS").ok().and_then(|v| v.parse().ok());
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let t = Instant::now();
    for k in 0..iters {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let a = (x % n as u64) as u32;
        let b = ((x >> 20) % n as u64) as u32;
        // CH_BENCH_MAKERS=k: makers drawn from k hot accounts (market makers)
        let b = match makers {
            Some(k) => b % k,
            None => b,
        };
        if a == b {
            continue;
        }
        let m = (k % 10) as u32;
        let side = if x & 1 == 0 { Side::Bid } else { Side::Ask };
        let px = Px((100 + m as u64) * 100_000_000);
        let sz = Sz(1_000_000);
        let c = MarginCheck { user: addr(a), asset: m, side, px, sz, reduce_only: false };
        if ch.check_order_margin(&c).is_ok() {
            let _ = ch.apply_fill(&FillParams {
                asset: m,
                px,
                sz,
                taker: addr(a),
                maker: addr(b),
                taker_side: side,
                liquidation_of: None,
                time: T0,
            });
        }
    }
    let el = t.elapsed();
    println!(
        "apply_fill + check_order_margin: {:.0} ns/op over {iters} ops ({n} accounts, 10 positions each)",
        el.as_nanos() as u128 / iters as u128
    );
    let t = Instant::now();
    let d = ch.take_dirty();
    println!("take_dirty: {} entities in {:?}", d.len(), t.elapsed());
}

#[test]
#[ignore]
fn bench_block_liquidation_scan() {
    let n = 100_000u32;
    let mut ch = setup(n);
    let blocks = 200u64;
    let t = Instant::now();
    let mut evaluated = 0usize;
    for b in 0..blocks {
        // ±0.2% random-walk on every market every block
        for m in 0..MARKETS {
            let base = (100 + m as u64) * 100_000_000;
            let wiggle = ((b * 7 + m as u64 * 13) % 9) as i64 - 4; // -4..4 → ±0.2% at 1e-8 units
            let p = Px((base as i64 + wiggle * base as i64 / 2000) as u64);
            ch.set_prices(m, p, p, None, T0 + b * 1000);
        }
        evaluated += ch.liquidation_targets().len();
        ch.on_block_funding(T0 + b * 1000);
    }
    println!(
        "per block (12 markets set_prices + liquidation_targets): {:?} avg, targets total {evaluated}",
        t.elapsed() / blocks as u32
    );
    let t = Instant::now();
    let full = ch.liquidation_targets_full_scan();
    println!("full scan for comparison: {:?} ({} targets)", t.elapsed(), full.len());
    let t = Instant::now();
    ch.check_invariants(&ExternalFlows { genesis: Usd(n as i64 * 1_000_000 * 1_000_000 + 1_000_000 * 1_000_000), ..Default::default() }, 64)
        .ok();
    println!("full invariant check: {:?}", t.elapsed());
}
