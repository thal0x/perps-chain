//! Micro benchmarks (ignored by default). Run with:
//!   cargo test -p hlwire --release --test bench -- --ignored --nocapture

use hlwire::*;
use interfaces::TxVerifier;
use serde_json::Value;
use std::hint::black_box;
use std::time::Instant;

fn vector(name: &str) -> Value {
    let p = format!("{}/testdata/python_vectors.json", env!("CARGO_MANIFEST_DIR"));
    let all: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    let v = all.as_array().unwrap().iter().find(|v| v["name"] == name).unwrap().clone();
    serde_json::from_str(v["body_raw"].as_str().unwrap()).unwrap()
}

/// Best of 10 rounds (the build machine is shared; the minimum is the
/// least-disturbed estimate).
fn time<F: FnMut()>(label: &str, iters: u32, mut f: F) -> f64 {
    for _ in 0..iters / 10 + 1 {
        f();
    }
    let mut best = f64::MAX;
    for _ in 0..10 {
        let t = Instant::now();
        for _ in 0..iters / 10 {
            f();
        }
        best = best.min(t.elapsed().as_secs_f64() * 1e6 / (iters / 10) as f64);
    }
    println!("{label:<40} {best:>9.2} us/op (best of 10)");
    best
}

#[test]
#[ignore]
fn bench_hlwire() {
    let body = vector("bulk_orders_50");
    let tx = parse_exchange_request(&body).unwrap();
    let ver = HlVerifier { is_mainnet: false };
    let action = decode_action(&tx.action_bytes).unwrap();
    let cid = l1_action_hash(&tx.action_bytes, tx.nonce, tx.vault_address, tx.expires_after);
    let digest = l1_signing_digest(cid, false);
    let w = Wallet::from_secret_hex("0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318").unwrap();
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
    if !cfg!(debug_assertions) {
        assert!(dec < 50.0, "decode_action too slow: {dec} us");
        assert!(rec < 40.0, "recover too slow: {rec} us");
    }
}
