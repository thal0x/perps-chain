//! Spot trading end to end (D25): hlwire-signed requests -> /exchange ->
//! the real `state::HlChain` (each submit executes one block) -> /info and
//! WebSocket views of PURR/USDC.
use api::testkit::{px, sz, test_genesis, usd};
use api::{ApiConfig, Shared};
use futures::{SinkExt, StreamExt};
use interfaces::*;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::Message;

const T0: u64 = 1_700_000_000_000;

struct ChainBackend {
    genesis: Genesis,
    chain: Mutex<state::HlChain>,
    callbacks: Mutex<Vec<BlockCallback>>,
}

impl ApiBackend for ChainBackend {
    fn genesis(&self) -> Genesis {
        self.genesis.clone()
    }
    fn query(&self, f: &mut dyn FnMut(&dyn StateQuery)) {
        let c = self.chain.lock();
        f(c.query());
    }
    fn submit(&self, tx: Tx, _source: TxSource) -> Result<TxHash, AdmissionError> {
        let h = tx.hash();
        let out = {
            let mut c = self.chain.lock();
            c.check_tx(&tx)?;
            let height = c.executed_height() + 1;
            let ctx = BlockContext { height, timestamp_ms: T0 + height * 1000, proposer: Address::ZERO, block_id: Hash32::ZERO };
            Arc::new(c.execute_block(&ctx, &[tx]))
        };
        for cb in self.callbacks.lock().iter() {
            cb(out.clone());
        }
        Ok(h)
    }
    fn on_block(&self, cb: BlockCallback) {
        self.callbacks.lock().push(cb);
    }
    fn status(&self) -> NodeStatus {
        NodeStatus::default()
    }
}

struct Env {
    base: String,
    ws: String,
    client: reqwest::Client,
    mm: hlwire::Wallet,
    taker: hlwire::Wallet,
    backend: Arc<ChainBackend>,
    nonce: u64,
}

impl Env {
    async fn info(&self, req: Value) -> Value {
        self.client.post(format!("{}/info", self.base)).json(&req).send().await.unwrap().json().await.unwrap()
    }
    async fn order(&mut self, who: usize, orders: &[OrderRequest]) -> Value {
        self.nonce += 1;
        let w = if who == 0 { &self.mm } else { &self.taker };
        let body = hlwire::signed_l1_request(w, hlwire::order_action(orders, Grouping::Na), self.nonce, None, None, false);
        self.client.post(format!("{}/exchange", self.base)).json(&body).send().await.unwrap().json().await.unwrap()
    }
    async fn cancel(&mut self, who: usize, oid: Oid) -> Value {
        self.nonce += 1;
        let w = if who == 0 { &self.mm } else { &self.taker };
        let body = hlwire::signed_l1_request(w, hlwire::cancel_action(&[(10_000, oid)]), self.nonce, None, None, false);
        self.client.post(format!("{}/exchange", self.base)).json(&body).send().await.unwrap().json().await.unwrap()
    }
}

fn spot_order(is_buy: bool, p: &str, s: &str, tif: Tif) -> OrderRequest {
    OrderRequest { asset: 10_000, is_buy, limit_px: px(p), sz: sz(s), reduce_only: false, kind: OrderKind::Limit { tif }, cloid: None }
}

async fn setup() -> Env {
    let mm = hlwire::Wallet::from_bytes(&[7u8; 32]).unwrap();
    let taker = hlwire::Wallet::from_bytes(&[8u8; 32]).unwrap();
    let mut g = test_genesis();
    g.genesis_time_ms = T0;
    g.validators = ValidatorSet { validators: vec![ValidatorInfo { address: Address([0x99; 20]), stake: 100, p2p_addr: String::new() }] };
    g.spot_balances = vec![
        (mm.address(), 0, usd("10000")),
        (mm.address(), 1, usd("10000")),
        (taker.address(), 0, usd("1000")),
        (taker.address(), 1, usd("5")),
    ];
    let chain = state::HlChain::new_hl(g.clone(), state::ChainConfig::strict());
    let backend = Arc::new(ChainBackend { genesis: g, chain: Mutex::new(chain), callbacks: Mutex::new(vec![]) });
    let shared = Shared::start(backend.clone(), ApiConfig::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = listener.local_addr().unwrap();
    tokio::spawn(api::serve_listener(shared, listener));
    Env {
        base: format!("http://{a}"),
        ws: format!("ws://{a}/ws"),
        client: reqwest::Client::new(),
        mm,
        taker,
        backend,
        nonce: T0 + 10,
    }
}

fn status0(r: &Value) -> &Value {
    assert_eq!(r["status"], "ok", "{r}");
    &r["response"]["data"]["statuses"][0]
}

#[tokio::test]
async fn spot_trading_end_to_end() {
    let mut e = setup().await;
    let mm_hex = e.mm.address().to_hex();
    let tk_hex = e.taker.address().to_hex();

    // spotMeta: PURR/USDC is pair 0 (asset 10000)
    let meta = e.info(json!({"type": "spotMeta"})).await;
    assert_eq!(meta["universe"][0], json!({"tokens": [1, 0], "name": "PURR/USDC", "index": 0, "isCanonical": true}));

    // WS: trades for PURR/USDC (basic_ws), l2Book, bbo
    let (mut ws, _) = tokio_tungstenite::connect_async(&e.ws).await.unwrap();
    for sub in [
        json!({"type": "trades", "coin": "PURR/USDC"}),
        json!({"type": "l2Book", "coin": "PURR/USDC"}),
        json!({"type": "userFills", "user": tk_hex}),
    ] {
        ws.send(Message::Text(json!({"method": "subscribe", "subscription": sub}).to_string().into())).await.unwrap();
    }

    // MM ladder: bids 0.99/0.98, asks 1.01/1.02
    let r = e
        .order(
            0,
            &[
                spot_order(true, "0.99", "100", Tif::Gtc),
                spot_order(true, "0.98", "100", Tif::Gtc),
                spot_order(false, "1.01", "100", Tif::Gtc),
                spot_order(false, "1.02", "100", Tif::Gtc),
            ],
        )
        .await;
    let st = r["response"]["data"]["statuses"].as_array().unwrap();
    assert!(st.iter().all(|s| s.get("resting").is_some()), "{r}");
    let l2 = e.info(json!({"type": "l2Book", "coin": "PURR/USDC"})).await;
    assert_eq!(l2["levels"][0][0], json!({"px": "0.99", "sz": "100.0", "n": 1}));
    assert_eq!(l2["levels"][1][0], json!({"px": "1.01", "sz": "100.0", "n": 1}));
    assert_eq!(e.info(json!({"type": "l2Book", "coin": "@0"})).await["levels"], l2["levels"]);
    let mids = e.info(json!({"type": "allMids"})).await;
    assert_eq!(mids["PURR/USDC"], "1.0");

    // holds: MM bids hold 99 + 98 + fee reserve, asks hold 200 PURR
    let sch = e.info(json!({"type": "spotClearinghouseState", "user": mm_hex})).await;
    let bal = |v: &Value, coin: &str| v["balances"].as_array().unwrap().iter().find(|b| b["coin"] == coin).cloned().unwrap();
    assert_eq!(bal(&sch, "USDC")["hold"], "197.06895");
    assert_eq!(bal(&sch, "PURR")["hold"], "200.0");
    let oo = e.info(json!({"type": "openOrders", "user": mm_hex})).await;
    assert_eq!(oo.as_array().unwrap().len(), 4);
    assert!(oo.as_array().unwrap().iter().all(|o| o["coin"] == "PURR/USDC"));

    // basic_spot_order: 24 @ 0.5 rests, orderStatus, cancel
    let r = e.order(1, &[spot_order(true, "0.5", "24", Tif::Gtc)]).await;
    let oid = status0(&r)["resting"]["oid"].as_u64().unwrap();
    let os = e.info(json!({"type": "orderStatus", "user": tk_hex, "oid": oid})).await;
    assert_eq!(os["order"]["order"]["coin"], "PURR/USDC");
    assert_eq!(os["order"]["status"], "open");
    assert_eq!(e.cancel(1, oid).await["response"]["data"]["statuses"][0], "success");

    // taker buys 150 (IOC): sweeps 1.01 and part of 1.02
    let r = e.order(1, &[spot_order(true, "1.02", "150", Tif::Ioc)]).await;
    assert_eq!(status0(&r)["filled"]["totalSz"], "150.0", "{r}");
    let fills = e.info(json!({"type": "userFills", "user": tk_hex})).await;
    let f = fills.as_array().unwrap();
    assert_eq!(f.len(), 2);
    assert_eq!(f[0]["coin"], "PURR/USDC");
    assert_eq!(f[0]["dir"], "Buy");
    assert_eq!(f[0]["closedPnl"], "0.0");
    assert_eq!(f[1]["startPosition"], "5.0");
    let mmf = e.info(json!({"type": "userFills", "user": mm_hex})).await;
    assert_eq!(mmf[0]["dir"], "Sell");
    let tr = e.info(json!({"type": "recentTrades", "coin": "PURR/USDC"})).await;
    assert_eq!(tr.as_array().unwrap().len(), 2);
    assert_eq!(tr[0]["coin"], "PURR/USDC");
    let c = e.info(json!({"type": "candleSnapshot", "req": {"coin": "PURR/USDC", "interval": "1m", "startTime": 0}})).await;
    assert_eq!(c[0]["v"], "150.0");
    let sch = e.info(json!({"type": "spotClearinghouseState", "user": tk_hex})).await;
    assert_eq!(bal(&sch, "PURR")["total"], "155.0");
    let ctxs = e.info(json!({"type": "spotMetaAndAssetCtxs"})).await;
    let ctx = &ctxs[1][0];
    assert_eq!(ctx["coin"], "PURR/USDC");
    assert_eq!(ctx["dayNtlVlm"], "152.0");
    assert_eq!(ctx["dayBaseVlm"], "150.0");
    assert_eq!(ctx["midPx"], "1.005");
    assert_eq!(ctx["markPx"], "1.02");

    // the chain's holds agree with the API's hold formula
    {
        let c = e.backend.chain.lock();
        let h = c.spot_hold(&e.mm.address());
        let api_hold = bal(&e_info_sync(&c, &e.mm.address()), "USDC")["hold"].clone();
        assert_eq!(api_hold, Value::from(fmt_usd(h.quote)));
    }

    // WS: trades on PURR/USDC arrive
    let mut got_trades = false;
    let mut got_book = false;
    let mut got_fills = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !(got_trades && got_book && got_fills) {
        let m = tokio::time::timeout_at(deadline, ws.next()).await.expect("ws messages").unwrap().unwrap();
        let Message::Text(t) = m else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
        match v["channel"].as_str() {
            Some("trades") => {
                assert_eq!(v["data"][0]["coin"], "PURR/USDC");
                got_trades = true;
            }
            Some("l2Book") => got_book = v["data"]["coin"] == "PURR/USDC",
            Some("userFills") => got_fills |= v["data"]["fills"].as_array().is_some_and(|f| !f.is_empty()),
            _ => {}
        }
    }
}

fn fmt_usd(u: Usd) -> String {
    api::fmt::usd(u)
}

/// spotClearinghouseState rendered by the API from the chain's query view.
fn e_info_sync(c: &state::HlChain, user: &Address) -> Value {
    let q = c.query();
    let coins = api::coins::Coins::new(c.genesis_ref());
    api::spot::clearinghouse_from(&coins, &c.genesis_ref().params, &q.account(user), &q.open_orders(user))
}
