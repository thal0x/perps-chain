mod common;
use api::testkit::*;
use api::ApiConfig;
use common::*;
use interfaces::*;
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test]
async fn info_market_data() {
    let s = start(ApiConfig::default()).await;
    let meta = s.info(json!({"type": "meta"})).await;
    assert_eq!(meta["universe"][0], json!({"szDecimals": 4, "name": "ETH", "maxLeverage": 50, "marginTableId": 50}));
    assert_eq!(meta["universe"][1]["name"], "BTC");
    assert_eq!(meta["collateralToken"], 0);
    assert_eq!(meta["marginTables"][0][0], 40);
    let m2 = s.info(json!({"type": "meta", "dex": ""})).await;
    assert_eq!(m2, meta);

    let mac = s.info(json!({"type": "metaAndAssetCtxs"})).await;
    let ctx = &mac[1][0];
    assert_eq!(ctx["markPx"], "3000.0");
    assert_eq!(ctx["oraclePx"], "3000.1");
    assert_eq!(ctx["funding"], "0.0000125");
    assert_eq!(ctx["premium"], "-0.0001");
    assert_eq!(ctx["openInterest"], "12.5");
    assert_eq!(ctx["prevDayPx"], "2950.0");
    assert_eq!(ctx["impactPxs"], json!(["2999.5", "3000.5"]));
    assert_eq!(mac[1][1]["impactPxs"], Value::Null);
    assert_eq!(mac[1][1]["midPx"], Value::Null);

    let spot = s.info(json!({"type": "spotMeta"})).await;
    assert_eq!(spot["universe"][0]["name"], "PURR/USDC");
    assert_eq!(spot["universe"][0]["tokens"], json!([1, 0]));
    assert_eq!(spot["universe"][1]["name"], "@1");
    assert_eq!(spot["tokens"][1]["tokenId"], "0xc4bf3f870c0e9465323c0b6ed28096c2");
    let smac = s.info(json!({"type": "spotMetaAndAssetCtxs"})).await;
    assert_eq!(smac[1].as_array().unwrap().len(), 2);
    assert_eq!(smac[1][0]["coin"], "PURR/USDC");

    let mids = s.info(json!({"type": "allMids"})).await;
    assert_eq!(mids, json!({"ETH": "3000.0", "BTC": "60000.0"}));
    assert_eq!(s.info(json!({"type": "allMids", "dex": "xyz"})).await, json!({}));

    let book = s.info(json!({"type": "l2Book", "coin": "ETH"})).await;
    assert_eq!(book["coin"], "ETH");
    assert!(book["time"].as_u64().unwrap() > 0);
    assert_eq!(book["levels"][0][0], json!({"px": "2999.5", "sz": "1.2", "n": 2}));
    assert_eq!(book["levels"][1][2], json!({"px": "3010.0", "sz": "2.0", "n": 3}));
    let agg = s.info(json!({"type": "l2Book", "coin": "ETH", "nSigFigs": 4})).await;
    assert_eq!(
        agg["levels"],
        json!([
            [{"px": "2999.0", "sz": "2.2", "n": 3}, {"px": "2990.0", "sz": "3.0", "n": 1}],
            [{"px": "3001.0", "sz": "0.9", "n": 2}, {"px": "3010.0", "sz": "2.0", "n": 3}]
        ])
    );
    let agg2 = s.info(json!({"type": "l2Book", "coin": "ETH", "nSigFigs": 2, "mantissa": null})).await;
    assert_eq!(agg2["levels"][0], json!([{"px": "2900.0", "sz": "5.2", "n": 4}]));
    assert_eq!(agg2["levels"][1], json!([{"px": "3100.0", "sz": "2.9", "n": 5}]));
    let agg5 = s.info(json!({"type": "l2Book", "coin": "ETH", "nSigFigs": 5, "mantissa": 5})).await;
    assert_eq!(agg5["levels"][0][0], json!({"px": "2999.5", "sz": "1.2", "n": 2}));
    assert_eq!(agg5["levels"][0][1], json!({"px": "2999.0", "sz": "1.0", "n": 1}));
    assert_eq!(s.info(json!({"type": "l2Book", "coin": "NOPE"})).await, Value::Null);
    let sb = s.info(json!({"type": "l2Book", "coin": "PURR/USDC"})).await;
    assert_eq!(sb["levels"], json!([[], []]));
    assert_eq!(s.info(json!({"type": "l2Book", "coin": "@1"})).await["coin"], "@1");

    let pf = s.info(json!({"type": "predictedFundings"})).await;
    assert_eq!(pf[0][0], "ETH");
    assert_eq!(pf[0][1][0][0], "HlPerp");
    assert_eq!(pf[0][1][0][1]["fundingRate"], "0.0000125");
    assert_eq!(s.info(json!({"type": "perpDexs"})).await, json!([null]));
    assert!(s.info(json!({"type": "exchangeStatus"})).await["time"].is_u64());
}

#[tokio::test]
async fn info_user_state() {
    let s = start(ApiConfig::default()).await;
    let a = user_a().to_hex();
    let ch = s.info(json!({"type": "clearinghouseState", "user": a, "dex": ""})).await;
    assert_eq!(ch["marginSummary"]["accountValue"], "10000.0");
    assert_eq!(ch["withdrawable"], "9925.0");
    assert_eq!(ch["crossMaintenanceMarginUsed"], "15.0");
    let p0 = &ch["assetPositions"][0];
    assert_eq!(p0["type"], "oneWay");
    assert_eq!(p0["position"]["coin"], "ETH");
    assert_eq!(p0["position"]["szi"], "0.5");
    assert_eq!(p0["position"]["leverage"], json!({"type": "cross", "value": 20}));
    assert_eq!(p0["position"]["liquidationPx"], "1500.5");
    assert_eq!(p0["position"]["cumFunding"]["allTime"], "1.25");
    let p1 = &ch["assetPositions"][1]["position"];
    assert_eq!(p1["szi"], "-0.01");
    assert_eq!(p1["leverage"], json!({"type": "isolated", "value": 10, "rawUsd": "75.0"}));
    // unknown user -> zeros
    let z = s.info(json!({"type": "clearinghouseState", "user": addr(77).to_hex()})).await;
    assert_eq!(z["marginSummary"]["accountValue"], "0.0");
    assert_eq!(z["assetPositions"], json!([]));
    // mixed-case address accepted
    let up = format!("0x{}", a[2..].to_uppercase());
    assert_eq!(s.info(json!({"type": "clearinghouseState", "user": up})).await["withdrawable"], "9925.0");

    let sp = s.info(json!({"type": "spotClearinghouseState", "user": a})).await;
    assert_eq!(sp["balances"][0], json!({"coin": "USDC", "token": 0, "total": "100.0", "hold": "0.0", "entryNtl": "0.0"}));
    assert_eq!(sp["balances"][1]["coin"], "PURR");

    let oo = s.info(json!({"type": "openOrders", "user": a})).await;
    let oo = oo.as_array().unwrap();
    assert_eq!(oo.len(), 2);
    let lim = oo.iter().find(|o| o["oid"] == 10).unwrap();
    assert_eq!(
        lim,
        &json!({"coin": "ETH", "side": "B", "limitPx": "1100.0", "sz": "0.2", "oid": 10, "timestamp": T0 - 1000, "origSz": "0.2", "cloid": "0x00000000000000000000000000000001"})
    );
    let trig = oo.iter().find(|o| o["oid"] == 11).unwrap();
    assert_eq!(trig["reduceOnly"], true);

    let fo = s.info(json!({"type": "frontendOpenOrders", "user": a})).await;
    let t = fo.as_array().unwrap().iter().find(|o| o["oid"] == 11).unwrap().clone();
    assert_eq!(t["orderType"], "Stop Market");
    assert_eq!(t["isTrigger"], true);
    assert_eq!(t["triggerPx"], "2600.0");
    assert_eq!(t["triggerCondition"], "Price below 2600.0");
    assert_eq!(t["tif"], Value::Null);
    let l = fo.as_array().unwrap().iter().find(|o| o["oid"] == 10).unwrap().clone();
    assert_eq!(l["orderType"], "Limit");
    assert_eq!(l["tif"], "Gtc");
    assert_eq!(l["triggerCondition"], "N/A");
    assert_eq!(l["children"], json!([]));

    let os = s.info(json!({"type": "orderStatus", "user": a, "oid": 10})).await;
    assert_eq!(os["status"], "order");
    assert_eq!(os["order"]["status"], "open");
    assert_eq!(os["order"]["order"]["oid"], 10);
    let oc = s.info(json!({"type": "orderStatus", "user": a, "oid": "0x00000000000000000000000000000001"})).await;
    assert_eq!(oc["order"]["order"]["oid"], 10);
    let unk = s.info(json!({"type": "orderStatus", "user": a, "oid": "0x00000000000000000000000000000002"})).await;
    assert_eq!(unk, json!({"status": "unknownOid"}));
    assert_eq!(s.info(json!({"type": "orderStatus", "user": a, "oid": 999})).await, json!({"status": "unknownOid"}));

    let aad = s.info(json!({"type": "activeAssetData", "user": a, "coin": "ETH"})).await;
    assert_eq!(aad["leverage"], json!({"type": "cross", "value": 20}));
    assert_eq!(aad["markPx"], "3000.0");
    assert_eq!(aad["availableToTrade"][0], "198500.0");
    assert_eq!(aad["maxTradeSzs"][0], "66.1666");
    let aad2 = s.info(json!({"type": "activeAssetData", "user": addr(50).to_hex(), "coin": "BTC"})).await;
    assert_eq!(aad2["leverage"], json!({"type": "cross", "value": 20}));

    let fees = s.info(json!({"type": "userFees", "user": a})).await;
    assert_eq!(fees["userCrossRate"], "0.00035");
    assert_eq!(fees["userAddRate"], "0.0001");
    let rl = s.info(json!({"type": "userRateLimit", "user": a})).await;
    assert_eq!(rl["nRequestsCap"], 10000);
    assert_eq!(s.info(json!({"type": "referral", "user": a})).await["referrerState"]["stage"], "needToCreateCode");
    assert_eq!(s.info(json!({"type": "maxBuilderFee", "user": a, "builder": a})).await, json!(0));
    assert_eq!(s.info(json!({"type": "userRole", "user": addr(9).to_hex()})).await["role"], "agent");
    assert_eq!(s.info(json!({"type": "userRole", "user": a})).await["role"], "user");
    let pw = s.info(json!({"type": "pendingWithdrawals"})).await;
    assert_eq!(
        pw,
        json!([{
            "id": 1,
            "user": a,
            "destination": a,
            "amount": "499",
            "created_at": T0,
            "signatures": [{"validator": addr(7).to_hex(), "r": format!("0x{}", "01".repeat(32)), "s": format!("0x{}", "02".repeat(32)), "v": 27}],
        }])
    );
    let ns = s.info(json!({"type": "nodeStatus"})).await;
    assert!(ns["committed_height"].is_u64());
    let wd = s.info(json!({"type": "webData2", "user": a})).await;
    assert_eq!(wd["user"], a);
    assert_eq!(wd["clearinghouseState"]["withdrawable"], "9925.0");
}

#[tokio::test]
async fn info_bad_requests_are_422() {
    let s = start(ApiConfig::default()).await;
    assert_eq!(s.info_status("not json").await, 422);
    assert_eq!(s.info_status(r#"{"type":"nonsense"}"#).await, 422);
    assert_eq!(s.info_status(r#"{"type":"clearinghouseState"}"#).await, 422);
    assert_eq!(s.info_status(r#"{"type":"clearinghouseState","user":"0x12"}"#).await, 422);
    assert_eq!(s.info_status(r#"{"type":"l2Book","coin":"ETH","nSigFigs":9}"#).await, 422);
    assert_eq!(s.info_status(r#"{"type":"candleSnapshot","req":{"coin":"ETH","interval":"7m","startTime":0}}"#).await, 422);
    assert_eq!(s.info_status(r#"[1,2,3]"#).await, 422);
    assert_eq!(s.info_status(r#"{"type":"orderStatus","user":"0x0000000000000000000000000000000000000001","oid":{}}"#).await, 422);
    // server still fine
    assert_eq!(s.info(json!({"type": "allMids"})).await["ETH"], "3000.0");
}

#[tokio::test]
async fn info_history_from_blocks() {
    let s = start(ApiConfig::default()).await;
    let a = user_a().to_hex();
    let b = user_b().to_hex();
    s.block(rich_events()).await;

    let fills = s.info(json!({"type": "userFills", "user": a})).await;
    let fills = fills.as_array().unwrap();
    assert_eq!(fills.len(), 3);
    // most recent first
    assert_eq!(fills[0]["tid"], 3);
    let f1 = fills.iter().find(|f| f["tid"] == 1).unwrap();
    assert_eq!(f1["coin"], "ETH");
    assert_eq!(f1["px"], "3000.5");
    assert_eq!(f1["sz"], "0.2");
    assert_eq!(f1["side"], "B");
    assert_eq!(f1["dir"], "Open Long");
    assert_eq!(f1["startPosition"], "0.0");
    assert_eq!(f1["crossed"], true);
    assert_eq!(f1["fee"], "0.105");
    assert_eq!(f1["feeToken"], "USDC");
    assert_eq!(f1["oid"], 101);
    assert_eq!(f1["closedPnl"], "0.0");
    assert_eq!(f1["cloid"], "0x00000000000000000000000000000001");
    assert_eq!(f1["hash"], Hash32([1; 32]).to_hex());
    assert_eq!(f1["twapId"], Value::Null);
    let f3 = fills.iter().find(|f| f["tid"] == 3).unwrap();
    assert_eq!(f3["side"], "A");
    assert_eq!(f3["liquidation"]["method"], "market");
    assert_eq!(f3["liquidation"]["liquidatedUser"], a);

    let bf = s.info(json!({"type": "userFills", "user": b})).await;
    let m1 = bf.as_array().unwrap().iter().find(|f| f["tid"] == 1).unwrap().clone();
    assert_eq!(m1["side"], "A");
    assert_eq!(m1["crossed"], false);
    assert_eq!(m1["dir"], "Close Long");
    assert_eq!(m1["fee"], "-0.03");
    assert_eq!(m1["closedPnl"], "1.5");
    assert!(m1.get("cloid").is_none());
    assert!(m1.get("liquidation").is_none());
    let m3 = bf.as_array().unwrap().iter().find(|f| f["tid"] == 3).unwrap().clone();
    assert_eq!(m3["side"], "B");
    assert_eq!(m3["dir"], "Open Long"); // maker start 0.2, buying

    // aggregateByTime merges tid 1 and 2? (different oids -> no)
    let agg = s.info(json!({"type": "userFills", "user": a, "aggregateByTime": true})).await;
    assert_eq!(agg.as_array().unwrap().len(), 3);
    let by_time = s.info(json!({"type": "userFillsByTime", "user": a, "startTime": T0 + 1})).await;
    assert_eq!(by_time.as_array().unwrap().len(), 1);
    let by_time = s.info(json!({"type": "userFillsByTime", "user": a, "startTime": 0, "endTime": T0})).await;
    assert_eq!(by_time.as_array().unwrap().len(), 2);
    assert_eq!(by_time[0]["tid"], 1); // ascending

    let ho = s.info(json!({"type": "historicalOrders", "user": a})).await;
    let ho = ho.as_array().unwrap();
    assert_eq!(ho.len(), 2);
    let o101 = ho.iter().find(|o| o["order"]["oid"] == 101).unwrap();
    assert_eq!(o101["status"], "filled");
    assert_eq!(o101["statusTimestamp"], T0 + 1);
    assert_eq!(o101["order"]["origSz"], "0.3");
    assert_eq!(o101["order"]["sz"], "0.0");
    let o102 = ho.iter().find(|o| o["order"]["oid"] == 102).unwrap();
    assert_eq!(o102["status"], "marginCanceled");

    let os = s.info(json!({"type": "orderStatus", "user": a, "oid": 101})).await;
    assert_eq!(os["order"]["status"], "filled");
    let os = s.info(json!({"type": "orderStatus", "user": a, "oid": "0x00000000000000000000000000000001"})).await;
    // live order with that cloid (oid 10) takes precedence
    assert_eq!(os["order"]["order"]["oid"], 10);

    let uf = s.info(json!({"type": "userFunding", "user": a, "startTime": 0})).await;
    assert_eq!(uf[0]["delta"], json!({"type": "funding", "coin": "ETH", "usdc": "-0.0123", "szi": "0.5", "fundingRate": "0.0000125", "nSamples": null}));
    assert_eq!(uf[0]["time"], T0);
    let fh = s.info(json!({"type": "fundingHistory", "coin": "ETH", "startTime": 0})).await;
    assert_eq!(fh, json!([{"coin": "ETH", "fundingRate": "0.0000125", "premium": "-0.0001", "time": T0}]));

    let led = s.info(json!({"type": "userNonFundingLedgerUpdates", "user": a, "startTime": 0})).await;
    let types: Vec<&str> = led.as_array().unwrap().iter().map(|l| l["delta"]["type"].as_str().unwrap()).collect();
    assert_eq!(types, vec!["deposit", "withdraw", "internalTransfer", "spotTransfer", "accountClassTransfer", "liquidation"]);
    assert_eq!(led[1]["delta"], json!({"type": "withdraw", "usdc": "500.0", "nonce": 7, "fee": "1.0"}));
    assert_eq!(led[3]["delta"]["token"], "PURR");
    assert_eq!(led[4]["delta"], json!({"type": "accountClassTransfer", "usdc": "1.23", "toPerp": false}));
    // receiving side sees transfers too
    let ledb = s.info(json!({"type": "userNonFundingLedgerUpdates", "user": b, "startTime": 0})).await;
    let tb: Vec<&str> = ledb.as_array().unwrap().iter().map(|l| l["delta"]["type"].as_str().unwrap()).collect();
    assert_eq!(tb, vec!["internalTransfer", "spotTransfer"]);
    let led_late = s.info(json!({"type": "userNonFundingLedgerUpdates", "user": a, "startTime": T0 + 1})).await;
    assert_eq!(led_late.as_array().unwrap().len(), 1);

    let rt = s.info(json!({"type": "recentTrades", "coin": "ETH"})).await;
    assert_eq!(rt[0]["tid"], 3);
    assert_eq!(rt[0]["users"], json!([b, a]));
    assert_eq!(rt[2]["side"], "B");
    assert_eq!(rt[2]["users"], json!([a, b]));

    let cs = s.info(json!({"type": "candleSnapshot", "req": {"coin": "ETH", "interval": "1m", "startTime": 0, "endTime": T0 + 10_000_000}})).await;
    assert_eq!(cs.as_array().unwrap().len(), 1);
    let c = &cs[0];
    assert_eq!(c["t"], 1_710_506_040_000u64);
    assert_eq!(c["T"], 1_710_506_099_999u64);
    assert_eq!(c["s"], "ETH");
    assert_eq!(c["i"], "1m");
    assert_eq!(c["o"], "3000.5");
    assert_eq!(c["c"], "2990.0");
    assert_eq!(c["h"], "3000.6");
    assert_eq!(c["l"], "2990.0");
    assert_eq!(c["v"], "0.4");
    assert_eq!(c["n"], 3);
    for iv in ["3m", "5m", "15m", "30m", "1h", "2h", "4h", "8h", "12h", "1d", "3d", "1w", "1M"] {
        let cs = s.info(json!({"type": "candleSnapshot", "req": {"coin": "ETH", "interval": iv, "startTime": 0}})).await;
        assert_eq!(cs.as_array().unwrap().len(), 1, "{iv}");
        assert_eq!(cs[0]["v"], "0.4");
    }
    let none = s.info(json!({"type": "candleSnapshot", "req": {"coin": "ETH", "interval": "1m", "startTime": T0 + 3_600_000}})).await;
    assert_eq!(none, json!([]));

    let fees = s.info(json!({"type": "userFees", "user": a})).await;
    assert_eq!(fees["dailyUserVlm"][0]["date"], "2024-03-15");
    assert_eq!(fees["dailyUserVlm"][0]["userCross"], "1199.16");
    assert_eq!(fees["dailyUserVlm"][0]["userAdd"], "0.0");
    assert_eq!(fees["dailyUserVlm"][0]["exchange"], "1199.16");
    let rl = s.info(json!({"type": "userRateLimit", "user": a})).await;
    assert_eq!(rl["cumVlm"], "1199.16");
    let mac = s.info(json!({"type": "metaAndAssetCtxs"})).await;
    let _ = mac; // dayBaseVlm depends on wall clock; checked in schema test
}

#[tokio::test]
async fn exchange_flow_and_rendering() {
    let s = start(ApiConfig::default()).await;
    let order = json!({"type": "order", "orders": [{"a": 0, "b": true, "p": "1100", "s": "0.2", "r": false, "t": {"limit": {"tif": "Gtc"}}}], "grouping": "na"});

    s.backend.script(Script::Execute {
        user: user_a(),
        result: Ok(ActionResponse::Order(vec![
            OrderStatusResp::Resting { oid: 1, cloid: None },
            OrderStatusResp::Resting { oid: 5, cloid: Some(cloid1()) },
            OrderStatusResp::Filled { total_sz: sz("0.02"), avg_px: px("1891.4"), oid: 2, cloid: None },
            OrderStatusResp::Filled { total_sz: sz("1"), avg_px: px("1891"), oid: 3, cloid: Some(cloid1()) },
            OrderStatusResp::Error("Order must have minimum value of $10.".into()),
            OrderStatusResp::WaitingForTrigger { oid: 4 },
            OrderStatusResp::WaitingForFill { oid: 6 },
        ])),
        events: vec![],
    });
    let r = s.exchange(order.clone()).await;
    assert_eq!(
        r,
        json!({"status": "ok", "response": {"type": "order", "data": {"statuses": [
            {"resting": {"oid": 1}},
            {"resting": {"oid": 5, "cloid": "0x00000000000000000000000000000001"}},
            {"filled": {"totalSz": "0.02", "avgPx": "1891.4", "oid": 2}},
            {"filled": {"totalSz": "1.0", "avgPx": "1891.0", "oid": 3, "cloid": "0x00000000000000000000000000000001"}},
            {"error": "Order must have minimum value of $10."},
            "waitingForTrigger",
            "waitingForFill"
        ]}}})
    );

    s.backend.script(Script::Execute {
        user: user_a(),
        result: Ok(ActionResponse::Cancel(vec![SimpleStatus::Success, SimpleStatus::Error("Order was never placed, already canceled, or filled. asset=0".into())])),
        events: vec![],
    });
    let r = s.exchange(json!({"type": "cancel", "cancels": [{"a": 0, "o": 1}, {"a": 0, "o": 2}]})).await;
    assert_eq!(
        r,
        json!({"status": "ok", "response": {"type": "cancel", "data": {"statuses": ["success", {"error": "Order was never placed, already canceled, or filled. asset=0"}]}}})
    );

    s.backend.script(Script::Execute { user: user_a(), result: Ok(ActionResponse::Default), events: vec![] });
    let r = s.exchange(json!({"type": "updateLeverage", "asset": 0, "isCross": true, "leverage": 21})).await;
    assert_eq!(r, json!({"status": "ok", "response": {"type": "default"}}));

    s.backend.script(Script::Execute { user: user_a(), result: Err("Insufficient margin to withdraw".into()), events: vec![] });
    let r = s.exchange(json!({"type": "withdraw3", "destination": user_a().to_hex(), "amount": "2", "time": 1})).await;
    assert_eq!(r, json!({"status": "err", "response": "Insufficient margin to withdraw"}));

    // Admission errors
    s.backend.script(Script::Reject(AdmissionError::UnknownSigner(addr(3))));
    let r = s.exchange(order.clone()).await;
    assert_eq!(r["status"], "err");
    assert_eq!(r["response"], format!("User or API Wallet {} does not exist.", addr(3).to_hex()));
    s.backend.script(Script::Reject(AdmissionError::BadNonce("Invalid nonce: duplicate nonce".into())));
    assert_eq!(s.exchange(order.clone()).await["response"], "Invalid nonce: duplicate nonce");
    s.backend.script(Script::Reject(AdmissionError::RateLimited));
    assert_eq!(s.exchange(order.clone()).await["status"], "err");

    // Malformed bodies
    let bad = s.client.post(format!("{}/exchange", s.base)).body("{nope").send().await.unwrap();
    assert_eq!(bad.status(), 200);
    let bad: Value = bad.json().await.unwrap();
    assert_eq!(bad["status"], "err");
    let bad: Value = s.client.post(format!("{}/exchange", s.base)).json(&json!({"action": {"type": "order"}})).send().await.unwrap().json().await.unwrap();
    assert_eq!(bad["status"], "err");

    // the submitted tx carried the API source ip and exact msgpack of the action
    let subs = s.backend.submitted.lock();
    let (tx, src) = &subs[0];
    assert_eq!(src, &TxSource::Api { ip: "127.0.0.1".into() });
    match tx {
        Tx::User(u) => {
            assert_eq!(u.action_bytes, hlwire::json_to_msgpack(&order).unwrap());
            assert_eq!(u.signature.v, 27);
        }
        _ => panic!(),
    }
    assert_eq!(s.shared.waiters.pending(), 0);
}

#[tokio::test]
async fn exchange_timeout_and_duplicate() {
    let cfg = ApiConfig { exchange_timeout: Duration::from_millis(300), ..Default::default() };
    let s = start(cfg).await;
    s.backend.script(Script::Drop);
    let t = std::time::Instant::now();
    let r = s.exchange(json!({"type": "noop"})).await;
    assert_eq!(r["status"], "err");
    assert!(t.elapsed() >= Duration::from_millis(300));
    assert_eq!(s.shared.waiters.pending(), 0);

    // duplicate of an already-executed tx returns the cached receipt
    let body = exchange_body(json!({"type": "scheduleCancel", "time": 123}));
    s.backend.script(Script::Execute { user: user_a(), result: Ok(ActionResponse::Default), events: vec![] });
    let r1: Value = s.client.post(format!("{}/exchange", s.base)).json(&body).send().await.unwrap().json().await.unwrap();
    assert_eq!(r1["status"], "ok");
    s.backend.script(Script::Reject(AdmissionError::Duplicate));
    let r2: Value = s.client.post(format!("{}/exchange", s.base)).json(&body).send().await.unwrap().json().await.unwrap();
    assert_eq!(r2, r1);

    // receipt arriving while waiting (block produced later by the node)
    s.backend.script(Script::Drop);
    let body = exchange_body(json!({"type": "scheduleCancel"}));
    let utx = api::wire::parse_exchange_request(&body).unwrap();
    let h = Tx::User(utx).hash();
    let fut = s.client.post(format!("{}/exchange", s.base)).json(&body).send();
    let backend = s.backend.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        backend.push_block(vec![TxReceipt { tx_hash: h, user: user_a(), result: Ok(ActionResponse::Default) }], vec![]);
    });
    let r: Value = fut.await.unwrap().json().await.unwrap();
    assert_eq!(r, json!({"status": "ok", "response": {"type": "default"}}));
}

#[tokio::test]
async fn exchange_concurrent_load() {
    let s = start(ApiConfig::default()).await;
    let mut handles = vec![];
    for i in 0..400u64 {
        let client = s.client.clone();
        let url = format!("{}/exchange", s.base);
        handles.push(tokio::spawn(async move {
            let orders: Vec<Value> = (0..50)
                .map(|j| json!({"a": 0, "b": true, "p": format!("{}", 1000 + j), "s": "0.01", "r": false, "t": {"limit": {"tif": "Gtc"}}}))
                .collect();
            let body = exchange_body(json!({"type": "order", "orders": orders, "grouping": "na", "i": i}));
            let r: Value = client.post(url).json(&body).send().await.unwrap().json().await.unwrap();
            r
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap()["status"], "ok");
    }
    assert_eq!(s.backend.submitted.lock().len(), 400);
}

#[tokio::test]
async fn admin_endpoints_and_cors() {
    let s = start(ApiConfig::default()).await;
    s.block(vec![]).await;
    let st: Value = s.client.get(format!("{}/admin/status", s.base)).send().await.unwrap().json().await.unwrap();
    assert_eq!(st["committed_height"], 1);
    assert!(st.get("exec_root").is_some());
    let ok = s.client.post(format!("{}/admin/chaos", s.base)).body(r#"{"drop":0.1}"#).send().await.unwrap();
    assert_eq!(ok.status(), 200);
    assert_eq!(s.backend.chaos.lock().as_deref(), Some(r#"{"drop":0.1}"#));
    let bad = s.client.post(format!("{}/admin/chaos", s.base)).body("garbage").send().await.unwrap();
    assert_eq!(bad.status(), 400);
    // CORS preflight
    let pre = s
        .client
        .request(reqwest::Method::OPTIONS, format!("{}/info", s.base))
        .header("Origin", "http://example.com")
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await
        .unwrap();
    assert!(pre.headers().get("access-control-allow-origin").is_some());
}

#[tokio::test]
async fn pending_withdrawals_roundtrip_through_relayer_parser() {
    let s = start(ApiConfig::default()).await;
    let pw = s.info(json!({"type": "pendingWithdrawals"})).await;
    let parsed = relayer::api::parse_pending_withdrawals(pw).unwrap();
    let expect = s.backend.state.lock().withdrawals.clone();
    assert_eq!(parsed, expect);
}

#[tokio::test]
async fn exchange_real_signed_requests_reach_backend_intact() {
    let s = start(ApiConfig::default()).await;
    let w = hlwire::Wallet::from_bytes(&[7u8; 32]).unwrap();
    let action = hlwire::order_action(
        &[OrderRequest {
            asset: 0,
            is_buy: true,
            limit_px: px("1100"),
            sz: sz("0.2"),
            reduce_only: false,
            kind: OrderKind::Limit { tif: Tif::Gtc },
            cloid: Some(cloid1()),
        }],
        Grouping::Na,
    );
    let l1 = hlwire::signed_l1_request(&w, action, 1_700_000_000_123, None, None, false);
    let r: Value = s.client.post(format!("{}/exchange", s.base)).json(&l1).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["status"], "ok");
    let us = hlwire::signed_user_request(&w, hlwire::usd_send_action(&user_b(), usd("1"), 1_700_000_000_124), false);
    let r: Value = s.client.post(format!("{}/exchange", s.base)).json(&us).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["status"], "ok");
    let subs = s.backend.submitted.lock();
    for (tx, _) in subs.iter() {
        let Tx::User(u) = tx else { panic!() };
        assert_eq!(hlwire::recover_signer(u, false).unwrap(), w.address());
    }
    assert_eq!(subs.len(), 2);
}
