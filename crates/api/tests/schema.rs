//! Validates every JSON shape this API emits against the response/event
//! types of the TS SDK `@nktkas/hyperliquid` (converted to JSON Schema with
//! `additionalProperties: false`, exactly like the SDK's own test-suite).
//!
//! Requires node (NODE env, /opt/node22/bin/node or PATH) and the SDK sources
//! (HL_TS_SDK, default /home/user/ext/hyperliquid). If either is absent the
//! test reports that and passes without validating.
mod common;
use api::testkit::*;
use api::ApiConfig;
use common::*;
use interfaces::*;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Samples(Vec<Value>);
impl Samples {
    fn add(&mut self, kind: &str, method: &str, label: &str, data: Value) {
        self.0.push(json!({"kind": kind, "method": method, "label": label, "data": data}));
    }
}

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

const WS_FILE: &[(&str, &str, &str)] = &[
    ("allMids", "sub", "allMids"),
    ("l2Book", "sub", "l2Book"),
    ("trades", "sub", "trades"),
    ("bbo", "sub", "bbo"),
    ("candle", "sub", "candle"),
    ("userFills", "sub", "userFills"),
    ("orderUpdates", "sub", "orderUpdates"),
    ("user", "sub", "userEvents"),
    ("userFundings", "sub", "userFundings"),
    ("userNonFundingLedgerUpdates", "sub", "userNonFundingLedgerUpdates"),
    ("notification", "sub", "notification"),
    ("activeAssetCtx", "sub", "activeAssetCtx"),
    ("activeSpotAssetCtx", "sub", "activeSpotAssetCtx"),
    ("activeAssetData", "sub", "activeAssetData"),
    ("clearinghouseState", "sub", "clearinghouseState"),
    ("openOrders", "sub", "openOrders"),
    ("webData2", "info", "webData2"),
];

#[tokio::test]
async fn outputs_match_ts_sdk_schemas() {
    let s = start(ApiConfig::default()).await;
    let a = user_a().to_hex();
    let b = user_b().to_hex();
    let mut sm = Samples(vec![]);
    // Spot (D25): PURR/USDC book, ctx and a resting spot buy of user A.
    {
        let mut st = s.backend.state.lock();
        st.books.insert(10_000, (vec![(px("0.99"), sz("100"), 2)], vec![(px("1.01"), sz("250"), 1)]));
        st.ctxs.insert(
            10_000,
            MarketCtx {
                asset: 10_000,
                oracle_px: px("1.005"),
                mark_px: px("1.005"),
                mid_px: Some(px("1")),
                funding: Rate::ZERO,
                premium: Rate::ZERO,
                open_interest: sz("0"),
                day_ntl_vlm: usd("0"),
                prev_day_px: px("1.005"),
            },
        );
        st.orders.get_mut(&user_a()).unwrap().push(order_view(10_000, 12, Side::Bid, "0.5", "24", None));
    }

    // ---- WS subscribers first so they see the history blocks
    let mut ws = ws_connect(&s.ws_url).await;
    let subs = vec![
        json!({"type": "allMids"}),
        json!({"type": "l2Book", "coin": "ETH", "nSigFigs": null, "mantissa": null}),
        json!({"type": "l2Book", "coin": "PURR/USDC"}),
        json!({"type": "trades", "coin": "ETH"}),
        json!({"type": "trades", "coin": "PURR/USDC"}),
        json!({"type": "bbo", "coin": "PURR/USDC"}),
        json!({"type": "candle", "coin": "PURR/USDC", "interval": "1m"}),
        json!({"type": "activeAssetCtx", "coin": "PURR/USDC"}),
        json!({"type": "bbo", "coin": "ETH"}),
        json!({"type": "bbo", "coin": "BTC"}),
        json!({"type": "candle", "coin": "ETH", "interval": "5m"}),
        json!({"type": "userFills", "user": a}),
        json!({"type": "orderUpdates", "user": a}),
        json!({"type": "userEvents", "user": a}),
        json!({"type": "userFundings", "user": a}),
        json!({"type": "userNonFundingLedgerUpdates", "user": a}),
        json!({"type": "notification", "user": a}),
        json!({"type": "webData2", "user": a}),
        json!({"type": "activeAssetCtx", "coin": "ETH"}),
        json!({"type": "activeAssetCtx", "coin": "@1"}),
        json!({"type": "activeAssetData", "user": a, "coin": "ETH"}),
        json!({"type": "clearinghouseState", "user": a, "dex": ""}),
        json!({"type": "openOrders", "user": a, "dex": ""}),
    ];
    let mut ws_msgs = vec![];
    for sub in &subs {
        ws_send(&mut ws, json!({"method": "subscribe", "subscription": sub})).await;
    }
    let mut acks = 0;
    while acks < subs.len() {
        let v = ws_next(&mut ws).await;
        if v["channel"] == "subscriptionResponse" {
            acks += 1;
        } else {
            ws_msgs.push(v);
        }
    }
    s.block(rich_events()).await;
    let mut spot_fill = fill_event(5, Side::Bid, "1.01", "20", T0 + 61_000);
    spot_fill.asset = 10_000;
    spot_fill.maker_closed_pnl = usd("0");
    spot_fill.taker_start_pos = sz("50");
    spot_fill.maker_start_pos = sz("1000");
    // a second block: more fills (different oid/time), funding
    s.block(vec![
        Event::Fill(fill_event(4, Side::Ask, "3001", "0.05", T0 + 60_000)),
        Event::Funding { user: user_a(), asset: 0, usdc: usd("0.5"), szi: sz("-0.5"), funding_rate: Rate::parse("-0.00001").unwrap(), time: T0 + 3_600_000 },
        Event::BookChanged { asset: 0 },
        Event::Fill(spot_fill),
        Event::BookChanged { asset: 10_000 },
    ])
    .await;
    // drain WS for a while
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(1500);
    while let Ok(v) = tokio::time::timeout(deadline.saturating_duration_since(tokio::time::Instant::now()), ws_next(&mut ws)).await {
        ws_msgs.push(v);
    }
    let mut channels_seen = std::collections::BTreeSet::new();
    for m in &ws_msgs {
        let ch = m["channel"].as_str().unwrap();
        if ch == "subscriptionResponse" {
            continue;
        }
        let Some((_, kind, method)) = WS_FILE.iter().find(|(c, _, _)| *c == ch) else { panic!("unmapped channel {ch}: {m}") };
        channels_seen.insert(ch.to_string());
        sm.add(kind, method, &format!("ws {ch}"), m["data"].clone());
    }
    for (c, _, _) in WS_FILE {
        assert!(channels_seen.contains(*c), "no WS message on {c}");
    }

    // ---- info
    let info_reqs: Vec<(&str, Value)> = vec![
        ("meta", json!({"type": "meta"})),
        ("metaAndAssetCtxs", json!({"type": "metaAndAssetCtxs"})),
        ("spotMeta", json!({"type": "spotMeta"})),
        ("spotMetaAndAssetCtxs", json!({"type": "spotMetaAndAssetCtxs"})),
        ("allMids", json!({"type": "allMids"})),
        ("l2Book", json!({"type": "l2Book", "coin": "ETH"})),
        ("l2Book", json!({"type": "l2Book", "coin": "ETH", "nSigFigs": 3})),
        ("l2Book", json!({"type": "l2Book", "coin": "PURR/USDC"})),
        ("l2Book", json!({"type": "l2Book", "coin": "NOPE"})),
        ("clearinghouseState", json!({"type": "clearinghouseState", "user": a})),
        ("clearinghouseState", json!({"type": "clearinghouseState", "user": b})),
        ("spotClearinghouseState", json!({"type": "spotClearinghouseState", "user": a})),
        ("spotClearinghouseState", json!({"type": "spotClearinghouseState", "user": b})),
        ("openOrders", json!({"type": "openOrders", "user": a})),
        ("frontendOpenOrders", json!({"type": "frontendOpenOrders", "user": a})),
        ("orderStatus", json!({"type": "orderStatus", "user": a, "oid": 10})),
        ("orderStatus", json!({"type": "orderStatus", "user": a, "oid": 11})),
        ("orderStatus", json!({"type": "orderStatus", "user": a, "oid": 101})),
        ("orderStatus", json!({"type": "orderStatus", "user": a, "oid": "0x00000000000000000000000000000009"})),
        ("userFills", json!({"type": "userFills", "user": a})),
        ("userFills", json!({"type": "userFills", "user": b, "aggregateByTime": true})),
        ("userFillsByTime", json!({"type": "userFillsByTime", "user": a, "startTime": 0})),
        ("userFunding", json!({"type": "userFunding", "user": a, "startTime": 0})),
        ("userNonFundingLedgerUpdates", json!({"type": "userNonFundingLedgerUpdates", "user": a, "startTime": 0})),
        ("userNonFundingLedgerUpdates", json!({"type": "userNonFundingLedgerUpdates", "user": b, "startTime": 0})),
        ("fundingHistory", json!({"type": "fundingHistory", "coin": "ETH", "startTime": 0})),
        ("historicalOrders", json!({"type": "historicalOrders", "user": a})),
        ("userFees", json!({"type": "userFees", "user": a})),
        ("userRateLimit", json!({"type": "userRateLimit", "user": a})),
        ("perpDexs", json!({"type": "perpDexs"})),
        ("referral", json!({"type": "referral", "user": a})),
        ("maxBuilderFee", json!({"type": "maxBuilderFee", "user": a, "builder": b})),
        ("recentTrades", json!({"type": "recentTrades", "coin": "ETH"})),
        ("recentTrades", json!({"type": "recentTrades", "coin": "PURR/USDC"})),
        ("l2Book", json!({"type": "l2Book", "coin": "@0"})),
        ("outcomeMeta", json!({"type": "outcomeMeta"})),
        ("activeAssetData", json!({"type": "activeAssetData", "user": a, "coin": "ETH"})),
        ("activeAssetData", json!({"type": "activeAssetData", "user": a, "coin": "BTC"})),
        ("predictedFundings", json!({"type": "predictedFundings"})),
        ("webData2", json!({"type": "webData2", "user": a})),
        ("exchangeStatus", json!({"type": "exchangeStatus"})),
        ("extraAgents", json!({"type": "extraAgents", "user": a})),
        ("userRole", json!({"type": "userRole", "user": a})),
        ("userRole", json!({"type": "userRole", "user": addr(9).to_hex()})),
        ("subAccounts", json!({"type": "subAccounts", "user": a})),
        ("userVaultEquities", json!({"type": "userVaultEquities", "user": a})),
        ("twapHistory", json!({"type": "twapHistory", "user": a})),
        ("userTwapSliceFills", json!({"type": "userTwapSliceFills", "user": a})),
        ("leadingVaults", json!({"type": "leadingVaults", "user": a})),
        ("delegations", json!({"type": "delegations", "user": a})),
    ];
    for (method, req) in info_reqs {
        let v = s.info(req.clone()).await;
        sm.add("info", method, &req.to_string(), v);
    }
    for iv in ["1m", "3m", "5m", "15m", "30m", "1h", "2h", "4h", "8h", "12h", "1d", "3d", "1w", "1M"] {
        let v = s.info(json!({"type": "candleSnapshot", "req": {"coin": "ETH", "interval": iv, "startTime": 0}})).await;
        sm.add("info", "candleSnapshot", iv, v);
    }
    let v = s.info(json!({"type": "candleSnapshot", "req": {"coin": "PURR/USDC", "interval": "1h", "startTime": 0}})).await;
    assert_eq!(v.as_array().map(|a| a.len()), Some(1), "spot candle: {v}");
    sm.add("info", "candleSnapshot", "PURR/USDC 1h", v);

    // ---- exchange responses
    let order_ok = ActionResponse::Order(vec![
        OrderStatusResp::Resting { oid: 1, cloid: None },
        OrderStatusResp::Resting { oid: 5, cloid: Some(cloid1()) },
        OrderStatusResp::Filled { total_sz: sz("0.02"), avg_px: px("1891.4"), oid: 2, cloid: None },
        OrderStatusResp::Filled { total_sz: sz("1"), avg_px: px("1891"), oid: 3, cloid: Some(cloid1()) },
        OrderStatusResp::Error("Order has invalid price.".into()),
        OrderStatusResp::WaitingForTrigger { oid: 4 },
        OrderStatusResp::WaitingForFill { oid: 6 },
    ]);
    let cancel_ok = ActionResponse::Cancel(vec![SimpleStatus::Success, SimpleStatus::Error("Order was never placed, already canceled, or filled.".into())]);
    let cases: Vec<(&str, Result<ActionResponse, String>)> = vec![
        ("order", Ok(order_ok.clone())),
        ("batchModify", Ok(order_ok)),
        ("cancel", Ok(cancel_ok.clone())),
        ("cancelByCloid", Ok(cancel_ok)),
        ("modify", Ok(ActionResponse::Default)),
        ("modify", Err("Cannot modify canceled or filled order".into())),
        ("scheduleCancel", Ok(ActionResponse::Default)),
        ("scheduleCancel", Err("Scheduled cancel time too early".into())),
        ("updateLeverage", Ok(ActionResponse::Default)),
        ("updateIsolatedMargin", Ok(ActionResponse::Default)),
        ("usdSend", Ok(ActionResponse::Default)),
        ("spotSend", Ok(ActionResponse::Default)),
        ("usdClassTransfer", Ok(ActionResponse::Default)),
        ("withdraw3", Ok(ActionResponse::Default)),
        ("withdraw3", Err("Insufficient balance".into())),
        ("approveAgent", Ok(ActionResponse::Default)),
        ("setReferrer", Ok(ActionResponse::Default)),
        ("noop", Ok(ActionResponse::Default)),
    ];
    for (method, result) in cases {
        s.backend.script(Script::Execute { user: user_a(), result, events: vec![] });
        let v = s.exchange(json!({"type": method})).await;
        sm.add("exchange", method, "", v);
    }

    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("api_schema_samples.json");
    std::fs::write(&out, serde_json::to_vec_pretty(&sm.0).unwrap()).unwrap();
    eprintln!("wrote {} samples to {}", sm.0.len(), out.display());

    let sdk = std::env::var("HL_TS_SDK").unwrap_or_else(|_| "/home/user/ext/hyperliquid".into());
    let Some(node) = node_bin() else {
        eprintln!("SKIP schema validation: node not found");
        return;
    };
    if !Path::new(&sdk).join("src/api/info/_methods").exists() {
        eprintln!("SKIP schema validation: TS SDK sources not found at {sdk}");
        return;
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/schema");
    if !dir.join("node_modules/ajv").exists() {
        let npm = node.parent().map(|p| p.join("npm")).filter(|p| p.exists()).unwrap_or_else(|| "npm".into());
        let mut path = std::env::var("PATH").unwrap_or_default();
        if let Some(p) = node.parent() {
            path = format!("{}:{path}", p.display());
        }
        let st = Command::new(npm).args(["ci", "--no-audit", "--no-fund"]).current_dir(&dir).env("PATH", path).status().expect("run npm");
        assert!(st.success(), "npm ci failed in {}", dir.display());
    }
    let o = Command::new(&node).arg(dir.join("validate.mjs")).arg(&out).arg(&sdk).output().expect("run node");
    let stdout = String::from_utf8_lossy(&o.stdout);
    eprintln!("{stdout}{}", String::from_utf8_lossy(&o.stderr));
    assert!(o.status.success(), "schema validation failed:\n{stdout}");
}
