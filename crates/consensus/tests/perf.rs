//! Throughput sanity check: 20k-tx blocks at the default 200 ms cadence.
//! `cargo test -p consensus --release --test perf -- --ignored --nocapture`

use consensus::sim::*;
use std::time::Instant;

fn run(n: usize, file: bool) -> (u64, std::time::Duration) {
    let mut cfg = SimConfig::new(n, 1);
    cfg.txs_per_block = 20_000;
    cfg.tx_bytes = 120;
    cfg.snapshot_every = 0;
    if file {
        let d = std::env::temp_dir().join(format!("hlbft-cons-perf-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        cfg.store = StoreKind::File { base: d, retain_heights: 0 };
    }
    let mut sim = Sim::new(cfg);
    let t = Instant::now();
    sim.run_for(10_000);
    let el = t.elapsed();
    sim.check_safety().unwrap();
    (sim.max_honest_committed(), el)
}

#[test]
#[ignore]
fn perf_20k_tx_blocks() {
    for (n, file) in [(1, false), (1, true), (4, false), (4, true)] {
        let (h, el) = run(n, file);
        println!(
            "n={n} file_store={file}: {h} blocks of 20k txs in 10 s virtual, {el:?} wall => {:.1} ms wall per block for all {n} nodes (sim overhead included)",
            el.as_secs_f64() * 1000.0 / h as f64
        );
    }
}

#[test]
#[ignore]
fn perf_primitives() {
    use interfaces::*;
    let payload: Vec<Tx> = (0..20_000u64)
        .map(|k| Tx::User(UserTx { action_bytes: vec![7; 120], nonce: k, signature: Sig65::default(), vault_address: None, expires_after: None }))
        .collect();
    let t = Instant::now();
    let h = hash_borsh(&payload);
    println!("hash_borsh: {:?} ({h})", t.elapsed());
    let t = Instant::now();
    let v = borsh::to_vec(&payload).unwrap();
    println!("borsh::to_vec: {:?} ({} bytes)", t.elapsed(), v.len());
    let t = Instant::now();
    let h2 = blake3_hash(&v);
    println!("blake3 of bytes: {:?} ({})", t.elapsed(), h2 == h);
    let t = Instant::now();
    let c = payload.clone();
    println!("clone: {:?} {}", t.elapsed(), c.len());
}
