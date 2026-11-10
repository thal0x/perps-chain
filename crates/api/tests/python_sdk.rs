//! End-to-end smoke test with the official Python SDK (Info, Exchange incl.
//! signing, WebSocket subscriptions of examples/basic_ws.py).
//!
//! Needs a Python with `hyperliquid-python-sdk` installed: set `HL_PYTHON`
//! (otherwise `python3` is tried). Skips (passes) if none can import it.
mod common;
use api::testkit::*;
use api::ApiConfig;
use common::*;
use std::process::Command;
use std::time::Duration;

fn python() -> Option<String> {
    let cands: Vec<String> = std::env::var("HL_PYTHON").map(|p| vec![p]).unwrap_or_else(|_| vec!["python3".into()]);
    cands.into_iter().find(|p| {
        Command::new(p).args(["-c", "import hyperliquid.info, eth_account"]).output().map(|o| o.status.success()).unwrap_or(false)
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn python_sdk_smoke() {
    let Some(py) = python() else {
        eprintln!("SKIP: no python with hyperliquid-python-sdk (set HL_PYTHON)");
        return;
    };
    let s = start(ApiConfig::default()).await;
    s.backend.set_responder(simple_responder(user_a()));
    // blocks keep flowing so pushes happen
    let backend = s.backend.clone();
    let pump = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            backend.push_block(vec![], rich_events());
        }
    });
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/python/sdk_smoke.py");
    let base = s.base.clone();
    let a = user_a().to_hex();
    let out = tokio::task::spawn_blocking(move || Command::new(py).arg(script).arg(base).arg(a).output().unwrap()).await.unwrap();
    pump.abort();
    let so = String::from_utf8_lossy(&out.stdout);
    let se = String::from_utf8_lossy(&out.stderr);
    eprintln!("{so}\n{se}");
    assert!(out.status.success(), "python sdk smoke failed");
    assert!(so.contains("ws ok"));
}
