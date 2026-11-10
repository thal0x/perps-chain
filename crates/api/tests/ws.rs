mod common;
use api::testkit::*;
use api::ApiConfig;
use common::*;
use interfaces::*;
use serde_json::{json, Value};

async fn sub(ws: &mut Ws, s: Value) -> Value {
    ws_send(ws, json!({"method": "subscribe", "subscription": s})).await;
    let r = ws_next(ws).await;
    assert_eq!(r["channel"], "subscriptionResponse", "{r}");
    assert_eq!(r["data"], json!({"method": "subscribe", "subscription": s}));
    r
}

#[tokio::test]
async fn ping_errors_and_unsubscribe() {
    let s = start(ApiConfig::default()).await;
    let mut ws = ws_connect(&s.ws_url).await;
    ws_send(&mut ws, json!({"method": "ping"})).await;
    assert_eq!(ws_next(&mut ws).await, json!({"channel": "pong"}));

    ws_send(&mut ws, json!({"method": "subscribe", "subscription": {"type": "l2Book", "coin": "NOPE"}})).await;
    let e = ws_next(&mut ws).await;
    assert_eq!(e["channel"], "error");
    assert!(e["data"].as_str().unwrap().starts_with("Invalid subscription"));
    ws_send(&mut ws, json!({"method": "subscribe", "subscription": {"type": "bogus"}})).await;
    assert_eq!(ws_next(&mut ws).await["channel"], "error");
    ws_send(&mut ws, json!({"method": "subscribe", "subscription": {"type": "userFills", "user": "0x12"}})).await;
    assert_eq!(ws_next(&mut ws).await["channel"], "error");

    ws.send_text("garbage").await;
    let e = ws_next(&mut ws).await;
    assert_eq!(e["channel"], "error");

    let ob = json!({"type": "orderUpdates", "user": user_a().to_hex()});
    sub(&mut ws, ob.clone()).await;
    ws_send(&mut ws, json!({"method": "subscribe", "subscription": ob})).await;
    let e = ws_next(&mut ws).await;
    assert!(e["data"].as_str().unwrap().starts_with("Already subscribed"), "{e}");
    ws_send(&mut ws, json!({"method": "unsubscribe", "subscription": ob})).await;
    assert_eq!(ws_next(&mut ws).await, json!({"channel": "subscriptionResponse", "data": {"method": "unsubscribe", "subscription": ob}}));
    ws_send(&mut ws, json!({"method": "unsubscribe", "subscription": ob})).await;
    assert!(ws_next(&mut ws).await["data"].as_str().unwrap().starts_with("Already unsubscribed"));
    // after unsubscribe nothing is pushed
    s.block(rich_events()).await;
    ws_quiet(&mut ws, 200).await;
    assert_eq!(s.shared.hub.n_subscriptions(), 0);
}

trait SendText {
    async fn send_text(&mut self, t: &str);
}
impl SendText for Ws {
    async fn send_text(&mut self, t: &str) {
        use futures::SinkExt;
        self.send(tokio_tungstenite::tungstenite::Message::Text(t.to_string().into())).await.unwrap();
    }
}

#[tokio::test]
async fn market_channels() {
    let s = start(ApiConfig::default()).await;
    let mut ws = ws_connect(&s.ws_url).await;

    sub(&mut ws, json!({"type": "allMids"})).await;
    let m = ws_next(&mut ws).await;
    assert_eq!(m, json!({"channel": "allMids", "data": {"mids": {"ETH": "3000.0", "BTC": "60000.0"}}}));

    // TS SDK sends nSigFigs/mantissa null explicitly
    sub(&mut ws, json!({"type": "l2Book", "coin": "ETH", "nSigFigs": null, "mantissa": null})).await;
    let b = ws_next(&mut ws).await;
    assert_eq!(b["channel"], "l2Book");
    assert_eq!(b["data"]["coin"], "ETH");
    assert_eq!(b["data"]["levels"][0][0]["px"], "2999.5");

    sub(&mut ws, json!({"type": "bbo", "coin": "ETH"})).await;
    let b = ws_next(&mut ws).await;
    assert_eq!(b["data"]["bbo"][0], json!({"px": "2999.5", "sz": "1.2", "n": 2}));
    assert_eq!(b["data"]["bbo"][1]["px"], "3000.5");

    sub(&mut ws, json!({"type": "trades", "coin": "ETH"})).await; // no trades yet -> no snapshot
    sub(&mut ws, json!({"type": "trades", "coin": "PURR/USDC"})).await;
    sub(&mut ws, json!({"type": "candle", "coin": "ETH", "interval": "1m"})).await; // no candle yet
    sub(&mut ws, json!({"type": "activeAssetCtx", "coin": "BTC"})).await;
    let c = ws_next(&mut ws).await;
    assert_eq!(c["channel"], "activeAssetCtx");
    assert_eq!(c["data"]["coin"], "BTC");
    sub(&mut ws, json!({"type": "activeAssetCtx", "coin": "@1"})).await;
    let c = ws_next(&mut ws).await;
    assert_eq!(c["channel"], "activeSpotAssetCtx");
    assert_eq!(c["data"]["coin"], "@1");
    assert_eq!(c["data"]["ctx"]["coin"], "@1");
    ws_quiet(&mut ws, 100).await;

    s.block(rich_events()).await;
    let mut got: Vec<Value> = vec![];
    // allMids, l2Book, bbo, trades, candle, activeAssetCtx, activeSpotAssetCtx (every block)
    for _ in 0..7 {
        got.push(ws_next(&mut ws).await);
    }
    let ch = |c: &str| got.iter().find(|m| m["channel"] == c).cloned().unwrap_or_else(|| panic!("no {c} in {got:?}"));
    assert_eq!(ch("allMids")["data"]["mids"]["ETH"], "3000.0");
    assert_eq!(ch("l2Book")["data"]["coin"], "ETH");
    assert_eq!(ch("bbo")["data"]["coin"], "ETH");
    let tr = ch("trades");
    assert_eq!(tr["data"].as_array().unwrap().len(), 3);
    assert_eq!(tr["data"][0]["coin"], "ETH");
    assert_eq!(tr["data"][0]["tid"], 1);
    let cd = ch("candle");
    assert_eq!(cd["data"]["s"], "ETH");
    assert_eq!(cd["data"]["i"], "1m");
    assert_eq!(cd["data"]["n"], 3);
    assert_eq!(ch("activeAssetCtx")["data"]["coin"], "BTC");
    ws_quiet(&mut ws, 150).await;

    // a block without book changes only pushes allMids + activeAssetCtx (perp and spot)
    s.block(vec![]).await;
    let mut chans = vec![];
    for _ in 0..3 {
        chans.push(ws_next(&mut ws).await["channel"].as_str().unwrap().to_string());
    }
    chans.sort();
    assert_eq!(chans, vec!["activeAssetCtx", "activeSpotAssetCtx", "allMids"]);
    ws_quiet(&mut ws, 150).await;

    // candle snapshot now exists for a new subscriber
    let mut ws2 = ws_connect(&s.ws_url).await;
    sub(&mut ws2, json!({"type": "candle", "coin": "ETH", "interval": "1h"})).await;
    assert_eq!(ws_next(&mut ws2).await["data"]["i"], "1h");
    sub(&mut ws2, json!({"type": "trades", "coin": "ETH"})).await;
    assert_eq!(ws_next(&mut ws2).await["data"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn user_channels() {
    let s = start(ApiConfig::default()).await;
    let a = user_a().to_hex();
    // pre-existing history for the snapshot
    s.block(vec![Event::Fill(fill_event(9, Side::Bid, "2000", "1", T0 - 5000))]).await;

    let mut ws = ws_connect(&s.ws_url).await;
    sub(&mut ws, json!({"type": "userFills", "user": a})).await;
    let snap = ws_next(&mut ws).await;
    assert_eq!(snap["channel"], "userFills");
    assert_eq!(snap["data"]["isSnapshot"], true);
    assert_eq!(snap["data"]["user"], a);
    assert_eq!(snap["data"]["fills"][0]["tid"], 9);

    sub(&mut ws, json!({"type": "userFundings", "user": a})).await;
    assert_eq!(ws_next(&mut ws).await["data"]["isSnapshot"], true);
    sub(&mut ws, json!({"type": "userNonFundingLedgerUpdates", "user": a})).await;
    let l = ws_next(&mut ws).await;
    assert_eq!(l["data"]["isSnapshot"], true);
    assert_eq!(l["data"]["nonFundingLedgerUpdates"], json!([]));
    sub(&mut ws, json!({"type": "clearinghouseState", "user": a})).await;
    let c = ws_next(&mut ws).await;
    assert_eq!(c["data"]["dex"], "");
    assert_eq!(c["data"]["user"], a);
    assert_eq!(c["data"]["clearinghouseState"]["withdrawable"], "9925.0");
    sub(&mut ws, json!({"type": "openOrders", "user": a, "dex": ""})).await;
    let o = ws_next(&mut ws).await;
    assert_eq!(o["data"]["orders"].as_array().unwrap().len(), 2);
    sub(&mut ws, json!({"type": "activeAssetData", "user": a, "coin": "ETH"})).await;
    let d = ws_next(&mut ws).await;
    assert_eq!(d["data"]["coin"], "ETH");
    assert_eq!(d["data"]["user"], a);
    sub(&mut ws, json!({"type": "webData2", "user": a})).await;
    assert_eq!(ws_next(&mut ws).await["data"]["user"], a);
    ws_quiet(&mut ws, 100).await;

    let mut ws_ev = ws_connect(&s.ws_url).await;
    sub(&mut ws_ev, json!({"type": "userEvents", "user": a})).await;
    sub(&mut ws_ev, json!({"type": "orderUpdates", "user": a})).await;
    sub(&mut ws_ev, json!({"type": "notification", "user": a})).await;
    ws_quiet(&mut ws_ev, 100).await;

    s.block(rich_events()).await;

    let mut got: Vec<Value> = vec![];
    for _ in 0..7 {
        got.push(ws_next(&mut ws).await);
    }
    let ch = |c: &str| got.iter().find(|m| m["channel"] == c).cloned().unwrap_or_else(|| panic!("no {c} in {got:?}"));
    let uf = ch("userFills");
    assert!(uf["data"].get("isSnapshot").is_none());
    assert_eq!(uf["data"]["fills"].as_array().unwrap().len(), 3);
    let fu = ch("userFundings");
    assert_eq!(fu["data"]["fundings"][0]["usdc"], "-0.0123");
    assert_eq!(fu["data"]["fundings"][0]["coin"], "ETH");
    let led = ch("userNonFundingLedgerUpdates");
    assert_eq!(led["data"]["nonFundingLedgerUpdates"].as_array().unwrap().len(), 6);
    ch("clearinghouseState");
    ch("openOrders");
    ch("activeAssetData");
    ch("webData2");

    let mut ev: Vec<Value> = vec![];
    for _ in 0..6 {
        ev.push(ws_next(&mut ws_ev).await);
    }
    let user_msgs: Vec<&Value> = ev.iter().filter(|m| m["channel"] == "user").collect();
    assert!(user_msgs.iter().any(|m| m["data"]["fills"].as_array().is_some_and(|f| f.len() == 3)));
    assert!(user_msgs.iter().any(|m| m["data"]["funding"]["fundingRate"] == "0.0000125"));
    assert!(user_msgs.iter().any(|m| m["data"]["liquidation"]["liquidated_user"] == a));
    assert!(user_msgs.iter().any(|m| m["data"]["nonUserCancel"] == json!([{"coin": "ETH", "oid": 102}])));
    let ou = ev.iter().find(|m| m["channel"] == "orderUpdates").unwrap();
    let ou = ou["data"].as_array().unwrap();
    assert_eq!(ou.len(), 3);
    assert_eq!(ou[1]["status"], "filled");
    assert_eq!(ou[1]["order"]["oid"], 101);
    assert_eq!(ou[1]["statusTimestamp"], T0 + 1);
    assert!(ev.iter().any(|m| m["channel"] == "notification"));
}

#[tokio::test]
async fn post_requests() {
    let s = start(ApiConfig::default()).await;
    let mut ws = ws_connect(&s.ws_url).await;
    ws_send(&mut ws, json!({"method": "post", "id": 7, "request": {"type": "info", "payload": {"type": "l2Book", "coin": "ETH"}}})).await;
    let r = ws_next(&mut ws).await;
    assert_eq!(r["channel"], "post");
    assert_eq!(r["data"]["id"], 7);
    assert_eq!(r["data"]["response"]["type"], "info");
    assert_eq!(r["data"]["response"]["payload"]["type"], "l2Book");
    assert_eq!(r["data"]["response"]["payload"]["data"]["coin"], "ETH");

    ws_send(&mut ws, json!({"method": "post", "id": 8, "request": {"type": "info", "payload": {"type": "nope"}}})).await;
    let r = ws_next(&mut ws).await;
    assert_eq!(r["data"]["id"], 8);
    assert_eq!(r["data"]["response"]["type"], "error");

    s.backend.script(Script::Execute {
        user: user_a(),
        result: Ok(ActionResponse::Order(vec![OrderStatusResp::Resting { oid: 42, cloid: None }])),
        events: vec![],
    });
    let body = exchange_body(json!({"type": "order", "orders": [], "grouping": "na"}));
    ws_send(&mut ws, json!({"method": "post", "id": 9, "request": {"type": "action", "payload": body}})).await;
    let r = ws_next(&mut ws).await;
    assert_eq!(r["data"]["id"], 9);
    assert_eq!(r["data"]["response"]["type"], "action");
    assert_eq!(r["data"]["response"]["payload"], json!({"status": "ok", "response": {"type": "order", "data": {"statuses": [{"resting": {"oid": 42}}]}}}));

    ws_send(&mut ws, json!({"method": "post", "id": 10, "request": {"type": "weird", "payload": {}}})).await;
    assert_eq!(ws_next(&mut ws).await["data"]["response"]["type"], "error");
}

#[tokio::test]
async fn ws_on_root_path_and_many_clients() {
    let s = start(ApiConfig::default()).await;
    let root = s.ws_url.trim_end_matches("/ws").to_string() + "/";
    let mut clients = vec![];
    for i in 0..20 {
        let mut ws = ws_connect(if i % 2 == 0 { &root } else { &s.ws_url }).await;
        sub(&mut ws, json!({"type": "allMids"})).await;
        ws_next(&mut ws).await;
        clients.push(ws);
    }
    s.block(vec![]).await;
    for ws in clients.iter_mut() {
        assert_eq!(ws_next(ws).await["channel"], "allMids");
    }
    drop(clients);
    // connections cleaned up
    for _ in 0..100 {
        if s.shared.hub.n_subscriptions() == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("subscriptions not cleaned up");
}
