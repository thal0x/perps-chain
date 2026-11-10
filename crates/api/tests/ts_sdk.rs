//! End-to-end smoke test with the TS SDK `@nktkas/hyperliquid` (npm, pinned
//! in tests/ts/package.json): README examples for info, exchange (HTTP and
//! WebSocket post) and subscriptions. Needs node + npm (NODE env,
//! /opt/node22/bin/node or PATH); skipped (passes) if node is absent.
mod common;
use api::testkit::*;
use api::ApiConfig;
use common::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn node_bin() -> Option<PathBuf> {
    if let Ok(n) = std::env::var("NODE") {
        return Some(n.into());
    }
    let p = PathBuf::from("/opt/node22/bin/node");
    if p.exists() {
        return Some(p);
    }
    Command::new("node").arg("--version").output().ok().filter(|o| o.status.success()).map(|_| "node".into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ts_sdk_smoke() {
    let Some(node) = node_bin() else {
        eprintln!("SKIP: node not found");
        return;
    };
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ts");
    if !dir.join("node_modules/@nktkas/hyperliquid").exists() {
        let npm = node.parent().map(|p| p.join("npm")).filter(|p| p.exists()).unwrap_or_else(|| "npm".into());
        let path = format!("{}:{}", node.parent().map(|p| p.display().to_string()).unwrap_or_default(), std::env::var("PATH").unwrap_or_default());
        let st = Command::new(npm).args(["install", "--no-audit", "--no-fund"]).current_dir(&dir).env("PATH", path).status().expect("npm");
        assert!(st.success(), "npm install failed");
    }
    let s = start(ApiConfig::default()).await;
    s.backend.set_responder(simple_responder(user_a()));
    let backend = s.backend.clone();
    let pump = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            backend.push_block(vec![], rich_events());
        }
    });
    let base = s.base.clone();
    let a = user_a().to_hex();
    let out = tokio::task::spawn_blocking(move || {
        Command::new(node).arg(dir.join("smoke.mjs")).arg(base).arg(a).current_dir(dir).output().unwrap()
    })
    .await
    .unwrap();
    pump.abort();
    let so = String::from_utf8_lossy(&out.stdout);
    eprintln!("{so}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "ts sdk smoke failed");
    assert!(so.contains("ws ok"));
}
