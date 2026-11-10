#![allow(dead_code)]
use api::testkit::*;
use api::{ApiConfig, Shared};
use futures::{SinkExt, StreamExt};
use interfaces::*;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

pub const T0: u64 = 1_710_506_096_789;

pub struct TestServer {
    pub base: String,
    pub ws_url: String,
    pub backend: Arc<FakeBackend>,
    pub shared: Arc<Shared>,
    pub client: reqwest::Client,
}

pub fn user_a() -> Address {
    addr(1)
}
pub fn user_b() -> Address {
    addr(2)
}

pub fn order_view(asset: AssetId, oid: Oid, side: Side, p: &str, s: &str, cloid: Option<Cloid>) -> RestingOrderView {
    RestingOrderView {
        asset,
        oid,
        side,
        limit_px: px(p),
        sz: sz(s),
        orig_sz: sz(s),
        timestamp: T0 - 1000,
        cloid,
        reduce_only: false,
        order_type: "Limit".into(),
        tif: Some(Tif::Gtc),
        trigger: None,
        is_trigger: false,
    }
}

pub fn cloid1() -> Cloid {
    let mut c = [0u8; 16];
    c[15] = 1;
    c
}

pub fn populate(b: &FakeBackend) {
    let mut st = b.state.lock();
    st.time = T0;
    st.ctxs.insert(
        0,
        MarketCtx {
            asset: 0,
            oracle_px: px("3000.1"),
            mark_px: px("3000"),
            mid_px: Some(px("3000")),
            funding: Rate::parse("0.0000125").unwrap(),
            premium: Rate::parse("-0.0001").unwrap(),
            open_interest: sz("12.5"),
            day_ntl_vlm: usd("123456.78"),
            prev_day_px: px("2950"),
        },
    );
    st.books.insert(
        0,
        (
            vec![(px("2999.5"), sz("1.2"), 2), (px("2999.4"), sz("1"), 1), (px("2990"), sz("3"), 1)],
            vec![(px("3000.5"), sz("0.8"), 1), (px("3000.6"), sz("0.1"), 1), (px("3010"), sz("2"), 3)],
        ),
    );
    let pos = |asset, szi: &str, lev| PositionView {
        asset,
        szi: sz(szi),
        entry_px: px("2900"),
        position_value: usd("1500"),
        unrealized_pnl: usd("-12.5"),
        return_on_equity: Rate::parse("-0.0125").unwrap(),
        leverage: lev,
        margin_used: usd("75"),
        liquidation_px: Some(px("1500.5")),
        max_leverage: 50,
        cum_funding_all_time: usd("1.25"),
        cum_funding_since_open: usd("-0.5"),
    };
    let ms = MarginSummary {
        account_value: usd("10000"),
        total_ntl_pos: usd("1500"),
        total_raw_usd: usd("8500"),
        total_margin_used: usd("75"),
    };
    st.accounts.insert(
        user_a(),
        AccountView {
            margin_summary: ms.clone(),
            cross_margin_summary: ms,
            cross_maintenance_margin_used: usd("15"),
            withdrawable: usd("9925"),
            positions: vec![pos(0, "0.5", Leverage::Cross(20)), pos(1, "-0.01", Leverage::Isolated(10))],
            spot_balances: vec![(0, usd("100")), (1, usd("50"))],
        },
    );
    let mut trig = order_view(0, 11, Side::Ask, "2500", "0.5", None);
    trig.trigger = Some((px("2600"), true, TpSl::Sl));
    trig.is_trigger = true;
    trig.reduce_only = true;
    trig.tif = None;
    st.orders.insert(user_a(), vec![order_view(0, 10, Side::Bid, "1100", "0.2", Some(cloid1())), trig]);
    st.agents.insert(addr(9), user_a());
    st.withdrawals.push(PendingWithdrawal {
        id: 1,
        user: user_a(),
        destination: user_a(),
        amount: usd("499"),
        created_at: T0,
        signatures: vec![(addr(7), Sig65 { r: [1; 32], s: [2; 32], v: 27 })],
    });
}

pub fn fill_event(tid: Tid, taker_side: Side, p: &str, s: &str, time: u64) -> FillEvent {
    FillEvent {
        asset: 0,
        tid,
        px: px(p),
        sz: sz(s),
        time,
        tx_hash: Hash32([tid as u8; 32]),
        taker: user_a(),
        maker: user_b(),
        taker_oid: 100 + tid,
        maker_oid: 200 + tid,
        taker_side,
        taker_fee: usd("0.105"),
        maker_fee: usd("-0.03"),
        taker_closed_pnl: usd("0"),
        maker_closed_pnl: usd("1.5"),
        taker_start_pos: sz("0"),
        maker_start_pos: sz("0.2"),
        taker_cloid: Some(cloid1()),
        maker_cloid: None,
        liquidation: None,
    }
}

/// A block with every event kind.
pub fn rich_events() -> Vec<Event> {
    let mut liq = fill_event(3, Side::Ask, "2990", "0.1", T0 + 2);
    liq.liquidation = Some(LiquidationInfo { liquidated_user: user_a(), mark_px: px("2991"), method: LiquidationMethod::Market });
    let mut filled = order_view(0, 101, Side::Bid, "3001", "0", Some(cloid1()));
    filled.orig_sz = sz("0.3");
    let canceled = order_view(0, 102, Side::Ask, "3500", "0.1", None);
    let h = Hash32([0x11; 32]);
    vec![
        Event::Fill(fill_event(1, Side::Bid, "3000.5", "0.2", T0)),
        Event::Fill(fill_event(2, Side::Bid, "3000.6", "0.1", T0)),
        Event::Fill(liq),
        Event::OrderUpdate { user: user_a(), order: order_view(0, 101, Side::Bid, "3001", "0.3", Some(cloid1())), status: OrderLifecycle::Open, time: T0 },
        Event::OrderUpdate { user: user_a(), order: filled, status: OrderLifecycle::Filled, time: T0 + 1 },
        Event::OrderUpdate { user: user_a(), order: canceled, status: OrderLifecycle::MarginCanceled, time: T0 + 1 },
        Event::Funding { user: user_a(), asset: 0, usdc: usd("-0.0123"), szi: sz("0.5"), funding_rate: Rate::parse("0.0000125").unwrap(), time: T0 },
        Event::Ledger { user: user_a(), time: T0, tx_hash: h, delta: LedgerDelta::Deposit { usdc: usd("1000") } },
        Event::Ledger { user: user_a(), time: T0, tx_hash: h, delta: LedgerDelta::Withdraw { usdc: usd("500"), nonce: 7, fee: usd("1") } },
        Event::Ledger {
            user: user_a(),
            time: T0,
            tx_hash: h,
            delta: LedgerDelta::InternalTransfer { usdc: usd("1"), user: user_a(), destination: user_b(), fee: usd("0") },
        },
        Event::Ledger {
            user: user_a(),
            time: T0,
            tx_hash: Hash32([0x12; 32]),
            delta: LedgerDelta::SpotTransfer {
                token: "PURR:0xc4bf3f870c0e9465323c0b6ed28096c2".into(),
                amount: usd("1"),
                user: user_a(),
                destination: user_b(),
                fee: usd("0"),
            },
        },
        Event::Ledger { user: user_a(), time: T0, tx_hash: h, delta: LedgerDelta::AccountClassTransfer { usdc: usd("1.23"), to_perp: false } },
        Event::Ledger {
            user: user_a(),
            time: T0 + 2,
            tx_hash: Hash32::ZERO,
            delta: LedgerDelta::Liquidation { account_value: usd("12.5"), liquidated_ntl_pos: usd("299") },
        },
        Event::BookChanged { asset: 0 },
    ]
}

pub async fn start(cfg: ApiConfig) -> TestServer {
    let backend = FakeBackend::new(test_genesis());
    populate(&backend);
    let shared = Shared::start(backend.clone(), cfg);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = listener.local_addr().unwrap();
    tokio::spawn(api::serve_listener(shared.clone(), listener));
    TestServer {
        base: format!("http://{a}"),
        ws_url: format!("ws://{a}/ws"),
        backend,
        shared,
        client: reqwest::Client::new(),
    }
}

impl TestServer {
    pub async fn info(&self, req: Value) -> Value {
        let r = self.client.post(format!("{}/info", self.base)).json(&req).send().await.unwrap();
        assert_eq!(r.status(), 200, "info {req}");
        assert!(r.headers()["content-type"].to_str().unwrap().contains("application/json"));
        r.json().await.unwrap()
    }

    pub async fn info_status(&self, req: &str) -> u16 {
        self.client.post(format!("{}/info", self.base)).body(req.to_string()).send().await.unwrap().status().as_u16()
    }

    pub async fn exchange(&self, action: Value) -> Value {
        let body = exchange_body(action);
        let r = self.client.post(format!("{}/exchange", self.base)).json(&body).send().await.unwrap();
        assert_eq!(r.status(), 200);
        r.json().await.unwrap()
    }

    /// Wait until the indexer has processed height `h`.
    pub async fn wait_height(&self, h: Height) {
        for _ in 0..400 {
            if self.shared.index.read().height() >= h {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("indexer never reached height {h}");
    }

    pub async fn block(&self, events: Vec<Event>) {
        let h = self.backend.push_block(vec![], events);
        self.wait_height(h).await;
    }
}

static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1_700_000_000_000);

pub fn exchange_body(action: Value) -> Value {
    let n = NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    json!({
        "action": action,
        "nonce": n,
        "signature": {"r": "0x1111111111111111111111111111111111111111111111111111111111111111", "s": "0x2222222222222222222222222222222222222222222222222222222222222222", "v": 27},
        "vaultAddress": null,
        "expiresAfter": null,
    })
}

pub type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub async fn ws_connect(url: &str) -> Ws {
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    // HL greets with a plain-text line.
    match tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap().unwrap().unwrap() {
        Message::Text(t) => assert_eq!(t.as_str(), "Websocket connection established."),
        m => panic!("unexpected {m:?}"),
    }
    ws
}

pub async fn ws_send(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

/// Next JSON message (fails after 5 s).
pub async fn ws_next(ws: &mut Ws) -> Value {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.expect("ws timeout").unwrap().unwrap();
        match m {
            Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
            Message::Ping(_) | Message::Pong(_) => continue,
            m => panic!("unexpected {m:?}"),
        }
    }
}

/// Next message on `channel` (skipping others), fails after 5 s.
pub async fn ws_next_on(ws: &mut Ws, channel: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let v = tokio::time::timeout(left, ws_next(ws)).await.unwrap_or_else(|_| panic!("no {channel} message"));
        if v["channel"] == channel {
            return v;
        }
    }
}

/// Assert no message arrives within `ms`.
pub async fn ws_quiet(ws: &mut Ws, ms: u64) {
    if let Ok(Some(Ok(m))) = tokio::time::timeout(Duration::from_millis(ms), ws.next()).await {
        panic!("unexpected message {m:?}");
    }
}

pub fn is_dec(v: &Value) -> bool {
    v.as_str().is_some_and(|s| {
        let s = s.strip_prefix('-').unwrap_or(s);
        let mut it = s.splitn(2, '.');
        let a = it.next().unwrap_or("");
        let b = it.next();
        !a.is_empty() && a.bytes().all(|c| c.is_ascii_digit()) && b.is_none_or(|b| !b.is_empty() && b.bytes().all(|c| c.is_ascii_digit()))
    })
}
