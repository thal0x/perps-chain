//! Micro benchmarks (ignored by default). Run with:
//!   cargo test -p hlwire --release --test bench -- --ignored --nocapture

use hlwire::*;
use interfaces::TxVerifier;
use serde_json::Value;
use std::hint::black_box;
use std::time::Instant;

fn vector(name: &str) -> Value {
    let p = format!(
        "{}/testdata/python_vectors.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let all: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    let v = all
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == name)
        .unwrap()
        .clone();
    serde_json::from_str(v["body_raw"].as_str().unwrap()).unwrap()
}

/// On-CPU time of the current thread in ns (Linux schedstat), so the
/// numbers are not inflated by other processes on the shared build box.
fn thread_cpu_ns() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    s.split_whitespace().next()?.parse().ok()
}

/// Best of 10 rounds of wall time and of on-CPU time; returns the CPU time
/// (falls back to wall time where schedstat is unavailable).
fn time<F: FnMut()>(label: &str, iters: u32, mut f: F) -> f64 {
    for _ in 0..iters / 10 + 1 {
        f();
    }
    let per = (iters / 10).max(1);
    let (mut best_wall, mut best_cpu) = (f64::MAX, f64::MAX);
    for _ in 0..10 {
        let c0 = thread_cpu_ns();
        let t = Instant::now();
        for _ in 0..per {
            f();
        }
        best_wall = best_wall.min(t.elapsed().as_secs_f64() * 1e6 / per as f64);
        if let (Some(a), Some(b)) = (c0, thread_cpu_ns()) {
            best_cpu = best_cpu.min((b - a) as f64 / 1e3 / per as f64);
        }
    }
    let cpu = if best_cpu == f64::MAX {
        best_wall
    } else {
        best_cpu
    };
    println!("{label:<40} {cpu:>9.2} us/op cpu  {best_wall:>9.2} us/op wall (best of 10)");
    cpu
}

#[test]
#[ignore]
fn bench_hlwire() {
    let body = vector("bulk_orders_50");
    let tx = parse_exchange_request(&body).unwrap();
    let ver = HlVerifier { is_mainnet: false };
    let action = decode_action(&tx.action_bytes).unwrap();
    let cid = l1_action_hash(
        &tx.action_bytes,
        tx.nonce,
        tx.vault_address,
        tx.expires_after,
    );
    let digest = l1_signing_digest(cid, false);
    let w = Wallet::from_secret_hex(
        "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318",
    )
    .unwrap();
    let body_txt = serde_json::to_string(&body).unwrap();

    let dec = time("decode_action (50 orders)", 20_000, || {
        black_box(decode_action(black_box(&tx.action_bytes)).unwrap());
    });
    let rec = time("recover", 5_000, || {
        black_box(recover(black_box(&digest), &tx.signature).unwrap());
    });
    time("sign_digest", 5_000, || {
        black_box(w.sign_digest(black_box(&digest)));
    });
    time("l1_action_hash + digest (50 orders)", 20_000, || {
        let c = l1_action_hash(black_box(&tx.action_bytes), tx.nonce, None, None);
        black_box(l1_signing_digest(c, false));
    });
    time("json_to_msgpack (50 orders)", 20_000, || {
        black_box(json_to_msgpack(black_box(&body["action"])).unwrap());
    });
    time("parse body text + parse_exchange_request", 5_000, || {
        let v: Value = serde_json::from_str(black_box(&body_txt)).unwrap();
        black_box(parse_exchange_request(&v).unwrap());
    });
    time("HlVerifier decode + recover_signer", 5_000, || {
        let a = ver.decode(black_box(&tx)).unwrap();
        black_box(ver.recover_signer(&tx, &a).unwrap());
    });
    black_box(action);
    // Targets: decode_action(50 orders) < 50 us, recover < 40 us. Timing on
    // the shared 4-core build box is noisy, so only HLWIRE_BENCH_STRICT=1
    // turns misses into failures.
    let ok_dec = dec < 50.0;
    let ok_rec = rec < 40.0;
    println!(
        "target decode_action < 50 us: {}",
        if ok_dec { "met" } else { "MISSED" }
    );
    println!(
        "target recover < 40 us: {}",
        if ok_rec { "met" } else { "MISSED" }
    );
    if std::env::var("HLWIRE_BENCH_STRICT").is_ok() && !cfg!(debug_assertions) {
        assert!(ok_dec, "decode_action too slow: {dec} us");
        assert!(ok_rec, "recover too slow: {rec} us");
    }
}

/// Fixed-work loop for instruction counting under callgrind, which is
/// immune to machine load:
///   valgrind --tool=callgrind <bench bin> --ignored --exact decode_only_loop
#[test]
#[ignore]
fn decode_only_loop() {
    let body = vector("bulk_orders_50");
    let tx = parse_exchange_request(&body).unwrap();
    for _ in 0..1000 {
        black_box(decode_action(black_box(&tx.action_bytes)).unwrap());
    }
}

#[test]
#[ignore]
fn exchange_body_loop() {
    let body = vector("bulk_orders_50");
    let txt = serde_json::to_string(&body).unwrap();
    for _ in 0..1000 {
        let v: Value = serde_json::from_str(black_box(&txt)).unwrap();
        black_box(parse_exchange_request(&v).unwrap());
    }
}
