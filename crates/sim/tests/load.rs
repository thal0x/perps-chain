//! `sim load` against an in-process mock `/exchange` that verifies every
//! request's signature like the API would.

use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[test]
fn load_bench_against_mock_exchange() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bad = Arc::new(AtomicU64::new(0));
    let good = Arc::new(AtomicU64::new(0));
    let (b2, g2) = (bad.clone(), good.clone());
    let addr = rt.block_on(async move {
        let app = Router::new().route(
            "/exchange",
            post(move |Json(body): Json<Value>| {
                let (bad, good) = (b2.clone(), g2.clone());
                async move {
                    let ok = hlwire::parse_exchange_request(&body).ok().and_then(|tx| hlwire::recover_signer(&tx, false).ok()).is_some();
                    if ok {
                        good.fetch_add(1, Ordering::Relaxed);
                        Json(json!({"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":1}}, {"error":"x"}]}}}))
                    } else {
                        bad.fetch_add(1, Ordering::Relaxed);
                        Json(json!({"status":"err","response":"bad sig"}))
                    }
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        addr
    });
    let args = sim::load::LoadArgs {
        targets: vec![format!("http://{addr}"), format!("http://{addr}/")],
        rate: std::env::var("SIM_LOAD_RATE").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000),
        duration: std::env::var("SIM_LOAD_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(2),
        mode: "bench".into(),
        keys: None,
        genesis: None,
        seed: 7,
        validators: 4,
        accounts: 40,
        concurrency: 256,
        presign_s: 1,
        invalid_ppm: 0,
        timeout_s: 10,
        sign_threads: 2,
        out: None,
    };
    let t = std::time::Instant::now();
    let r = sim::load::run_load(&args).unwrap();
    eprintln!("load took {:?}: {r:?}", t.elapsed());
    assert!(r.submitted > 100, "{r:?}");
    assert_eq!(r.accepted, r.submitted, "{r:?}");
    assert_eq!(bad.load(Ordering::Relaxed), 0);
    assert_eq!(good.load(Ordering::Relaxed), r.submitted);
    assert!(r.order_actions_submitted >= args.rate * args.duration * 3 / 4, "{r:?}");
    assert_eq!(r.entry_errors, r.accepted);
    assert!(r.latency_ms.p50 > 0.0);
}
