//! /exchange throughput over the fake backend (receipt per tx, immediate
//! block): `cargo run --release -p api --example bench_exchange -- [conns] [secs]`.
//! Each request carries a 50-order batch.
use api::testkit::*;
use api::{ApiConfig, Shared};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() {
    let conns: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(64);
    let secs: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let backend = FakeBackend::new(test_genesis());
    backend.set_responder(simple_responder(addr(1)));
    let shared = Shared::start(backend.clone(), ApiConfig::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/exchange", listener.local_addr().unwrap());
    tokio::spawn(api::serve_listener(shared, listener));
    let orders: Vec<_> = (0..50)
        .map(|j| json!({"a": 0, "b": true, "p": format!("{}", 1000 + j), "s": "0.01", "r": false, "t": {"limit": {"tif": "Gtc"}}}))
        .collect();
    let template = json!({
        "action": {"type": "order", "orders": orders, "grouping": "na"},
        "nonce": 0,
        "signature": {"r": "0x01", "s": "0x02", "v": 27},
        "vaultAddress": null,
    })
    .to_string()
    .replace("\"nonce\":0", "\"nonce\":NONCE");
    let done = Arc::new(AtomicU64::new(0));
    let nonce = Arc::new(AtomicU64::new(1_700_000_000_000));
    let end = Instant::now() + Duration::from_secs(secs);
    let mut hs = vec![];
    for _ in 0..conns {
        let (url, template, done, nonce) = (url.clone(), template.clone(), done.clone(), nonce.clone());
        hs.push(tokio::spawn(async move {
            let c = reqwest::Client::new();
            while Instant::now() < end {
                let body = template.replace("NONCE", &nonce.fetch_add(1, Ordering::Relaxed).to_string());
                let r = c.post(&url).body(body).header("content-type", "application/json").send().await.unwrap();
                let _ = r.bytes().await.unwrap();
                done.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for h in hs {
        h.await.unwrap();
    }
    let n = done.load(Ordering::Relaxed);
    let cpu = cpu_secs();
    println!("process cpu {cpu:.2}s => {:.0} us cpu/request (client+server)", cpu * 1e6 / n.max(1) as f64);
    println!("{n} requests in {secs}s = {:.0} req/s ({:.0} orders/s), {conns} connections", n as f64 / secs as f64, 50.0 * n as f64 / secs as f64);
    drop(backend);
}

fn cpu_secs() -> f64 {
    // utime + stime from /proc/self/stat (clock ticks, assume 100 Hz)
    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let rest = s.rsplit(')').next().unwrap_or("");
    let f: Vec<&str> = rest.split_whitespace().collect();
    let t = |i: usize| f.get(i).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0);
    (t(11) + t(12)) / 100.0
}
