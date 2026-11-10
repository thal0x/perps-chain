//! AC3 cost breakdown, offline and single-threaded (ignored by default):
//!
//! ```text
//! cargo test --release -p node --test exec_bench -- --ignored --nocapture
//! ```
//! env: BENCH_SECS (10) of traffic at BENCH_RATE (100000) order actions/s,
//! BENCH_BLOCK_MS (300) per block, BENCH_ACCOUNTS (200).
//!
//! Generates `sim load --mode bench` traffic (MM batches of 50 + IOC takers),
//! then times, per stage, exactly what every validator does for every tx:
//! JSON parse of the `/exchange` body (API node only), `Tx::hash` (borsh +
//! blake3), action decode (msgpack), ECDSA recovery, `check_tx` under the
//! chain read lock, mempool admission, payload hashing, and block execution
//! (`execute_block`, signatures pre-cached) split into execute vs. root commit
//! is not possible from outside the state crate, so execution is one number.

use interfaces::*;
use sim::agents::{AgentKind, Mode, PopParams, Population};
use sim::app::World;
use sim::sign::{sign_all, SignCtx};
use std::time::Instant;

type Blocks = Vec<(u64, Vec<(Tx, Vec<u8>)>, u64)>;

fn env(k: &str, d: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

#[test]
#[ignore]
fn ac3_cost_breakdown() {
    let secs = env("BENCH_SECS", 10);
    let rate = env("BENCH_RATE", 100_000);
    let block_ms = env("BENCH_BLOCK_MS", 300);
    let n_acc = env("BENCH_ACCOUNTS", 200) as usize;
    let (mut g, mut keys) = sim::genesis(42, 4, n_acc, &sim::default_markets());
    let mm = n_acc * 4 / 5;
    keys.kinds = (0..n_acc).map(|i| if i < mm { AgentKind::MarketMaker } else { AgentKind::Taker }).collect();
    // fund everybody like an MM (bench accounts are well funded in testnet-gen --bench too)
    for b in g.balances.iter_mut() {
        b.1 = Usd::from_raw(5_000_000 * 1_000_000);
    }
    let start_ts = g.genesis_time_ms + 1000;
    let mut pop = Population::new(&g, &keys, PopParams { seed: 42, mode: Mode::Bench, invalid_ppm: 0, skewed_validators: 0, validators: false });
    let world = World::new(g.markets.len(), keys.accounts.len(), false);
    let cx = SignCtx { keys: &keys, chain_id: g.chain_id, is_mainnet: g.is_mainnet, want_body: true };
    let blocks_n = secs * 1000 / block_ms;
    let per_block = rate * block_ms / 1000;
    let t = Instant::now();
    let mut blocks: Blocks = Vec::new();
    for b in 0..blocks_n {
        let ts = start_ts + b * block_ms;
        let us = pop.step(b + 1, ts, per_block, &world, &keys);
        let signed = sign_all(&cx, &us);
        let oa: u64 = signed.iter().map(|s| s.order_actions).sum();
        blocks.push((ts, signed.into_iter().filter_map(|s| s.body.map(|bd| (s.tx, bd))).collect(), oa));
    }
    let gen_s = t.elapsed().as_secs_f64();
    let n_tx: usize = blocks.iter().map(|b| b.1.len()).sum();
    let n_oa: u64 = blocks.iter().map(|b| b.2).sum();
    let body_bytes: usize = blocks.iter().flat_map(|b| b.1.iter().map(|x| x.1.len())).sum();
    let borsh_bytes: usize = blocks.iter().flat_map(|b| b.1.iter().map(|x| borsh::to_vec(&x.0).unwrap().len())).sum();
    println!(
        "traffic: {blocks_n} blocks, {n_tx} txs, {n_oa} order actions ({:.1}/tx), body {:.0} B/tx, borsh {:.0} B/tx; gen+sign {gen_s:.1}s",
        n_oa as f64 / n_tx as f64,
        body_bytes as f64 / n_tx as f64,
        borsh_bytes as f64 / n_tx as f64
    );

    let verifier = hlwire::HlVerifier::new(g.is_mainnet);
    let mut timings: Vec<(&str, f64)> = Vec::new();
    let mut time = |name: &'static str, f: &mut dyn FnMut()| {
        let t = Instant::now();
        f();
        let s = t.elapsed().as_secs_f64();
        timings.push((name, s));
    };
    // API parse (/exchange body -> UserTx)
    time("api: parse /exchange JSON body", &mut || {
        for b in &blocks {
            for (_, body) in &b.1 {
                std::hint::black_box(api::wire::parse_exchange_bytes(body).unwrap());
            }
        }
    });
    time("Tx::hash (borsh + blake3), x1", &mut || {
        for b in &blocks {
            for (tx, _) in &b.1 {
                std::hint::black_box(tx.hash());
            }
        }
    });
    time("decode action (msgpack)", &mut || {
        for b in &blocks {
            for (tx, _) in &b.1 {
                if let Tx::User(u) = tx {
                    std::hint::black_box(verifier.decode(u).unwrap());
                }
            }
        }
    });
    let decoded: Vec<Vec<Action>> = blocks
        .iter()
        .map(|b| b.1.iter().map(|(tx, _)| if let Tx::User(u) = tx { verifier.decode(u).unwrap() } else { unreachable!() }).collect())
        .collect();
    time("ECDSA recover signer", &mut || {
        for (b, d) in blocks.iter().zip(&decoded) {
            for ((tx, _), a) in b.1.iter().zip(d) {
                if let Tx::User(u) = tx {
                    std::hint::black_box(verifier.recover_signer(u, a).unwrap());
                }
            }
        }
    });

    // chain with bench config (invariants off)
    let ccfg = state::ChainConfig { invariants: state::InvariantMode::Never, ..Default::default() };
    let mut chain = state::HlChain::new_hl(g.clone(), ccfg);
    let cache = chain.sig_cache();
    let prime = |blocks: &Blocks| {
        for b in blocks {
            for (tx, _) in &b.1 {
                if let Tx::User(u) = tx {
                    let a = verifier.decode(u).unwrap();
                    let s = verifier.recover_signer(u, &a).unwrap();
                    cache.insert(tx.hash(), std::sync::Arc::new(Ok((a, s))));
                }
            }
        }
    };
    prime(&blocks);
    time("check_tx (cached sig; nonce/state checks)", &mut || {
        for b in &blocks {
            for (tx, _) in &b.1 {
                let _ = std::hint::black_box(interfaces::Application::check_tx(&chain, tx));
            }
        }
    });
    let pool = mempool::Pool::new(
        mempool::PoolConfig::default(),
        mempool::PoolDeps::new(std::sync::Arc::new(|_tx: &Tx| Ok(()))),
    );
    time("mempool insert_prechecked", &mut || {
        for b in &blocks {
            for (tx, _) in &b.1 {
                let _ = interfaces::Mempool::insert(&*pool, tx.clone(), TxSource::Local);
            }
        }
    });
    time("payload hash (consensus, once per block)", &mut || {
        for b in &blocks {
            let txs: Vec<Tx> = b.1.iter().map(|x| x.0.clone()).collect();
            std::hint::black_box(consensus::crypto::payload_hash(&txs));
        }
    });
    let block_txs: Vec<Vec<Tx>> = blocks.iter().map(|b| b.1.iter().map(|x| x.0.clone()).collect()).collect();
    // oracle prices so marks exist
    let mut exec_s = 0.0;
    let mut per_block = Vec::new();
    let mut total_oa = 0;
    let mut indexer = api::indexer::Indexer::default();
    let mut index_s = 0.0;
    let (mut n_fills, mut n_orders, mut n_events) = (0usize, 0usize, 0usize);
    for (i, (b, txs)) in blocks.iter().zip(&block_txs).enumerate() {
        let ctx = BlockContext { height: i as u64 + 1, timestamp_ms: b.0, proposer: g.validators.validators[0].address, block_id: Hash32::ZERO };
        let t = Instant::now();
        let o = interfaces::Application::execute_block(&mut chain, &ctx, txs);
        let s = t.elapsed().as_secs_f64();
        exec_s += s;
        total_oa += o.order_actions;
        per_block.push((s * 1000.0, o.order_actions, o.receipts.iter().filter(|r| r.result.is_err()).count()));
        n_events += o.events.len();
        n_fills += o.events.iter().filter(|e| matches!(e, Event::Fill(_))).count();
        n_orders += o.events.iter().filter(|e| matches!(e, Event::OrderUpdate { .. })).count();
        let t = Instant::now();
        std::hint::black_box(indexer.ingest(&o, &|_| Rate::ZERO));
        index_s += t.elapsed().as_secs_f64();
    }
    timings.push(("execute_block (sig cached)", exec_s));
    timings.push(("api Indexer::ingest (every block)", index_s));
    println!("events {n_events}: fills {n_fills}, order updates {n_orders}");
    println!("executed order actions {total_oa} of {n_oa}; first blocks (ms, order actions, err receipts): {:?}", &per_block[..per_block.len().min(5)]);
    println!("last blocks: {:?}", &per_block[per_block.len().saturating_sub(3)..]);
    let errs: Vec<String> = Vec::new();
    let _ = errs;
    println!("\nper-validator CPU cost of {rate} order actions/s ({:.0} txs/s), single core:", n_tx as f64 / secs as f64);
    for (n, s) in &timings {
        println!("  {n:45} {:8.1} ms per second of traffic  ({:6.2} us/tx)", s * 1000.0 / secs as f64, s * 1e6 / n_tx as f64);
    }
}
