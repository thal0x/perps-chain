//! WebSocket: subscriptions, pushes driven by executed blocks, `post`
//! requests (info and exchange actions) and ping/pong.

use crate::candle::Interval;
use crate::coins::CoinRef;
use crate::fmt;
use crate::indexer::BlockDelta;
use crate::info;
use crate::render;
use crate::shared::Shared;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket};
use futures::{SinkExt, StreamExt};
use interfaces::*;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub type ConnId = u64;

/// Canonical subscription identity (one per channel/params combination).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SubKey {
    AllMids { dex: Option<String> },
    L2Book { coin: String, n_sig: Option<u32>, mantissa: Option<u32> },
    Trades { coin: String },
    Bbo { coin: String },
    Candle { coin: String, interval: Interval },
    OrderUpdates { user: Address },
    UserFills { user: Address, aggregate: bool },
    UserEvents { user: Address },
    UserFundings { user: Address },
    UserNonFundingLedgerUpdates { user: Address },
    Notification { user: Address },
    WebData2 { user: Address },
    ActiveAssetCtx { coin: String },
    ActiveAssetData { user: Address, coin: String },
    ClearinghouseState { user: Address, dex: String },
    OpenOrders { user: Address, dex: String },
}

impl SubKey {
    pub fn user(&self) -> Option<&Address> {
        match self {
            SubKey::OrderUpdates { user }
            | SubKey::UserFills { user, .. }
            | SubKey::UserEvents { user }
            | SubKey::UserFundings { user }
            | SubKey::UserNonFundingLedgerUpdates { user }
            | SubKey::Notification { user }
            | SubKey::WebData2 { user }
            | SubKey::ActiveAssetData { user, .. }
            | SubKey::ClearinghouseState { user, .. }
            | SubKey::OpenOrders { user, .. } => Some(user),
            _ => None,
        }
    }
}

/// Parse and validate a subscription object.
pub fn parse_sub(s: &Shared, v: &Value) -> Option<SubKey> {
    let ty = v.get("type")?.as_str()?;
    let user = || v.get("user").and_then(|u| u.as_str()).and_then(fmt::parse_addr);
    let coin = || -> Option<String> {
        let c = v.get("coin")?.as_str()?;
        s.coins.resolve(c)?;
        Some(c.to_string())
    };
    let opt_u32 = |k: &str| -> Result<Option<u32>, ()> {
        match v.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(x) => x.as_u64().map(|n| Some(n as u32)).ok_or(()),
        }
    };
    let dex = || match v.get("dex") {
        None | Some(Value::Null) => Some(String::new()),
        Some(Value::String(d)) => Some(d.clone()),
        _ => None,
    };
    Some(match ty {
        "allMids" => SubKey::AllMids {
            dex: match v.get("dex") {
                None | Some(Value::Null) => None,
                Some(Value::String(d)) => Some(d.clone()),
                _ => return None,
            },
        },
        "l2Book" => {
            let n_sig = opt_u32("nSigFigs").ok()?;
            if let Some(n) = n_sig {
                if !(2..=5).contains(&n) {
                    return None;
                }
            }
            SubKey::L2Book { coin: coin()?, n_sig, mantissa: opt_u32("mantissa").ok()? }
        }
        "trades" => SubKey::Trades { coin: coin()? },
        "bbo" => SubKey::Bbo { coin: coin()? },
        "candle" => SubKey::Candle { coin: coin()?, interval: Interval::parse(v.get("interval")?.as_str()?)? },
        "orderUpdates" => SubKey::OrderUpdates { user: user()? },
        "userFills" => SubKey::UserFills {
            user: user()?,
            aggregate: v.get("aggregateByTime").and_then(|x| x.as_bool()).unwrap_or(false),
        },
        "userEvents" => SubKey::UserEvents { user: user()? },
        "userFundings" => SubKey::UserFundings { user: user()? },
        "userNonFundingLedgerUpdates" => SubKey::UserNonFundingLedgerUpdates { user: user()? },
        "notification" => SubKey::Notification { user: user()? },
        "webData2" => SubKey::WebData2 { user: user()? },
        "activeAssetCtx" => SubKey::ActiveAssetCtx { coin: coin()? },
        "activeAssetData" => {
            let c = coin()?;
            s.coins.perp(&c)?;
            SubKey::ActiveAssetData { user: user()?, coin: c }
        }
        "clearinghouseState" => SubKey::ClearinghouseState { user: user()?, dex: dex()? },
        "openOrders" => SubKey::OpenOrders { user: user()?, dex: dex()? },
        _ => return None,
    })
}

pub fn msg(channel: &str, data: Value) -> String {
    json!({"channel": channel, "data": data}).to_string()
}

/// One WebSocket connection's outbound side.
pub struct Conn {
    pub id: ConnId,
    tx: mpsc::Sender<Utf8Bytes>,
    dead: AtomicBool,
    cancel: CancellationToken,
}

impl Conn {
    /// Queue a message; a full queue kills the connection (slow consumer).
    pub fn send(&self, m: &Utf8Bytes) -> bool {
        if self.dead.load(Ordering::Relaxed) {
            return false;
        }
        match self.tx.try_send(m.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::debug!(conn = self.id, "ws slow consumer dropped");
                self.kill();
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.kill();
                false
            }
        }
    }
    pub fn send_str(&self, s: String) -> bool {
        self.send(&Utf8Bytes::from(s))
    }
    pub fn kill(&self) {
        self.dead.store(true, Ordering::Relaxed);
        self.cancel.cancel();
    }
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
pub struct Hub {
    subs: RwLock<HashMap<SubKey, HashMap<ConnId, Arc<Conn>>>>,
    next_id: AtomicU64,
    /// Serializes block publication against subscribe snapshots so a
    /// subscriber sees each block exactly once, after its snapshot.
    pub(crate) publish_lock: Mutex<()>,
}

impl Hub {
    pub fn is_empty(&self) -> bool {
        self.subs.read().is_empty()
    }

    pub fn n_subscriptions(&self) -> usize {
        self.subs.read().values().map(|m| m.len()).sum()
    }

    fn add(&self, k: SubKey, c: Arc<Conn>) {
        self.subs.write().entry(k).or_default().insert(c.id, c);
    }

    fn remove(&self, k: &SubKey, id: ConnId) {
        let mut g = self.subs.write();
        if let Some(m) = g.get_mut(k) {
            m.remove(&id);
            if m.is_empty() {
                g.remove(k);
            }
        }
    }

    /// Push the updates caused by an executed block.
    pub fn publish(&self, s: &Shared, b: &BlockOutcome, d: &BlockDelta) {
        let subs: Vec<(SubKey, Vec<Arc<Conn>>)> = {
            let g = self.subs.read();
            if g.is_empty() {
                return;
            }
            g.iter().map(|(k, m)| (k.clone(), m.values().cloned().collect())).collect()
        };
        let mut cache = PushCache::default();
        let mut dead: Vec<(SubKey, ConnId)> = Vec::new();
        for (k, conns) in subs {
            let msgs = update_msgs(s, &k, b, d, &mut cache);
            for m in msgs {
                let m = Utf8Bytes::from(m);
                for c in &conns {
                    c.send(&m);
                }
            }
            for c in &conns {
                if c.is_dead() {
                    dead.push((k.clone(), c.id));
                }
            }
        }
        if !dead.is_empty() {
            let mut g = self.subs.write();
            for (k, id) in dead {
                if let Some(m) = g.get_mut(&k) {
                    m.remove(&id);
                    if m.is_empty() {
                        g.remove(&k);
                    }
                }
            }
        }
    }
}

#[derive(Default)]
struct PushCache {
    all_mids: Option<Value>,
    asset_ctx: HashMap<AssetId, Value>,
}

/// Perp asset or tradable spot asset (10000 + pair) of a coin.
fn coin_asset(s: &Shared, coin: &str) -> Option<AssetId> {
    match s.coins.resolve(coin)? {
        CoinRef::Perp(a) => Some(a),
        CoinRef::Spot(p) => s.coins.tradable_spot_asset(p),
    }
}

/// Messages to send to subscribers of `k` after block `b`.
fn update_msgs(s: &Shared, k: &SubKey, _b: &BlockOutcome, d: &BlockDelta, cache: &mut PushCache) -> Vec<String> {
    let coins = &s.coins;
    let now = s.now();
    let mut out = Vec::new();
    match k {
        SubKey::AllMids { dex } => {
            let mids = cache.all_mids.get_or_insert_with(|| info::all_mids(s)).clone();
            out.push(all_mids_msg(mids, dex));
        }
        SubKey::L2Book { coin, n_sig, mantissa } => {
            if coin_asset(s, coin).is_some_and(|a| d.book_changed.contains(&a)) {
                if let Some(v) = info::l2_book(s, coin, *n_sig, *mantissa) {
                    out.push(msg("l2Book", v));
                }
            }
        }
        SubKey::Bbo { coin } => {
            if coin_asset(s, coin).is_some_and(|a| d.book_changed.contains(&a)) {
                if let Some(v) = info::bbo(s, coin) {
                    out.push(msg("bbo", v));
                }
            }
        }
        SubKey::Trades { coin } => {
            if let Some(ts) = coin_asset(s, coin).and_then(|a| d.trades_by_asset.get(&a)) {
                out.push(msg("trades", Value::Array(ts.iter().map(|t| render::trade(coins, t)).collect())));
            }
        }
        SubKey::Candle { coin, interval } => {
            if let Some(a) = coin_asset(s, coin) {
                if d.candles_changed.contains(&(a, *interval)) {
                    if let Some(c) = s.index.read().last_candle(a, *interval) {
                        out.push(msg("candle", render::candle(coin, *interval, &c)));
                    }
                }
            }
        }
        SubKey::OrderUpdates { user } => {
            if let Some(v) = d.orders_by_user.get(user) {
                out.push(msg("orderUpdates", Value::Array(v.iter().map(|r| render::order_update(coins, r)).collect())));
            }
        }
        SubKey::UserFills { user, aggregate } => {
            if let Some(v) = d.fills_by_user.get(user) {
                let v = if *aggregate { render::aggregate_fills(v.clone()) } else { v.clone() };
                let fills: Vec<Value> = v.iter().map(|f| render::fill(coins, f)).collect();
                out.push(msg("userFills", json!({"user": fmt::addr(user), "fills": fills})));
            }
        }
        SubKey::UserEvents { user } => {
            if let Some(v) = d.fills_by_user.get(user) {
                out.push(msg("user", json!({"fills": v.iter().map(|f| render::fill(coins, f)).collect::<Vec<_>>()})));
            }
            if let Some(v) = d.funding_by_user.get(user) {
                for f in v {
                    out.push(msg("user", json!({"funding": render::funding_ws(coins, f)})));
                }
            }
            if let Some(v) = d.ledger_by_user.get(user) {
                for l in v {
                    if let LedgerDelta::Liquidation { account_value, liquidated_ntl_pos } = &l.delta {
                        out.push(msg(
                            "user",
                            json!({"liquidation": {
                                "lid": d.height,
                                "liquidator": fmt::addr(&Address::ZERO),
                                "liquidated_user": fmt::addr(user),
                                "liquidated_ntl_pos": fmt::usd_abs(*liquidated_ntl_pos),
                                "liquidated_account_value": fmt::usd(*account_value),
                            }}),
                        ));
                    }
                }
            }
            if let Some(v) = d.orders_by_user.get(user) {
                let nuc: Vec<Value> = v
                    .iter()
                    .filter(|r| render::is_non_user_cancel(r.status))
                    .map(|r| json!({"coin": coins.name(r.order.asset), "oid": r.order.oid}))
                    .collect();
                if !nuc.is_empty() {
                    out.push(msg("user", json!({ "nonUserCancel": nuc })));
                }
            }
        }
        SubKey::UserFundings { user } => {
            if let Some(v) = d.funding_by_user.get(user) {
                let f: Vec<Value> = v.iter().map(|f| render::funding_ws(coins, f)).collect();
                out.push(msg("userFundings", json!({"user": fmt::addr(user), "fundings": f})));
            }
        }
        SubKey::UserNonFundingLedgerUpdates { user } => {
            if let Some(v) = d.ledger_by_user.get(user) {
                let l: Vec<Value> = v.iter().map(|l| render::ledger(coins, l)).collect();
                out.push(msg("userNonFundingLedgerUpdates", json!({"user": fmt::addr(user), "nonFundingLedgerUpdates": l})));
            }
        }
        SubKey::Notification { user } => {
            if let Some(v) = d.ledger_by_user.get(user) {
                for l in v {
                    if let LedgerDelta::Liquidation { account_value, .. } = &l.delta {
                        out.push(msg(
                            "notification",
                            json!({"notification": format!("Your account was liquidated (account value {}).", fmt::usd(*account_value))}),
                        ));
                    }
                }
            }
        }
        SubKey::WebData2 { user } => out.push(msg("webData2", info::web_data2(s, user, now))),
        SubKey::ActiveAssetCtx { coin } => match coins.resolve(coin) {
            Some(CoinRef::Perp(a)) => {
                let ctx = cache.asset_ctx.entry(a).or_insert_with(|| info::asset_ctx_one(s, a, now)).clone();
                out.push(msg("activeAssetCtx", json!({"coin": coin, "ctx": ctx})));
            }
            Some(CoinRef::Spot(p)) => {
                let key = crate::coins::SPOT_ASSET_OFFSET + p;
                let ctx = cache.asset_ctx.entry(key).or_insert_with(|| crate::spot::ctx(s, p, now)).clone();
                out.push(msg("activeSpotAssetCtx", json!({"coin": coin, "ctx": ctx})));
            }
            None => {}
        },
        SubKey::ActiveAssetData { user, coin } => {
            if let Some(v) = info::active_asset_data(s, user, coin) {
                out.push(msg("activeAssetData", v));
            }
        }
        SubKey::ClearinghouseState { user, dex } => {
            out.push(msg("clearinghouseState", clearinghouse_data(s, user, dex, now)));
        }
        SubKey::OpenOrders { user, dex } => {
            if d.orders_by_user.contains_key(user) || d.fills_by_user.contains_key(user) {
                out.push(msg("openOrders", open_orders_data(s, user, dex)));
            }
        }
    }
    out
}

fn all_mids_msg(mids: Value, dex: &Option<String>) -> String {
    let mids = match dex {
        Some(d) if !d.is_empty() => json!({}),
        _ => mids,
    };
    let mut data = json!({ "mids": mids });
    if let Some(d) = dex {
        data["dex"] = d.clone().into();
    }
    msg("allMids", data)
}

fn clearinghouse_data(s: &Shared, user: &Address, dex: &str, now: u64) -> Value {
    let a = if dex.is_empty() { s.with_state(|q| q.account(user)) } else { AccountView::default() };
    json!({"dex": dex, "user": fmt::addr(user), "clearinghouseState": render::clearinghouse(&s.coins, &a, now)})
}

fn open_orders_data(s: &Shared, user: &Address, dex: &str) -> Value {
    let mut v = if dex.is_empty() { s.with_state(|q| q.open_orders(user)) } else { vec![] };
    v.sort_by(|a, b| (b.timestamp, b.oid).cmp(&(a.timestamp, a.oid)));
    let orders: Vec<Value> = v.iter().map(|o| render::frontend_order(&s.coins, o)).collect();
    json!({"dex": dex, "user": fmt::addr(user), "orders": orders})
}

/// Initial messages sent right after a successful subscribe.
pub fn snapshot_msgs(s: &Shared, k: &SubKey) -> Vec<String> {
    let coins = &s.coins;
    let now = s.now();
    let mut out = Vec::new();
    match k {
        SubKey::AllMids { dex } => out.push(all_mids_msg(info::all_mids(s), dex)),
        SubKey::L2Book { coin, n_sig, mantissa } => {
            if let Some(v) = info::l2_book(s, coin, *n_sig, *mantissa) {
                out.push(msg("l2Book", v));
            }
        }
        SubKey::Bbo { coin } => {
            if let Some(v) = info::bbo(s, coin) {
                out.push(msg("bbo", v));
            }
        }
        SubKey::Trades { coin } => {
            if let Some(a) = coin_asset(s, coin) {
                let mut v = s.index.read().recent_trades(a, 50);
                if !v.is_empty() {
                    v.reverse();
                    out.push(msg("trades", Value::Array(v.iter().map(|t| render::trade(coins, t)).collect())));
                }
            }
        }
        SubKey::Candle { coin, interval } => {
            if let Some(a) = coin_asset(s, coin) {
                if let Some(c) = s.index.read().last_candle(a, *interval) {
                    out.push(msg("candle", render::candle(coin, *interval, &c)));
                }
            }
        }
        SubKey::UserFills { user, aggregate } => {
            let mut v = s.index.read().user_fills(user, 2000);
            if *aggregate {
                v.reverse();
                v = render::aggregate_fills(v);
                v.reverse();
            }
            let fills: Vec<Value> = v.iter().map(|f| render::fill(coins, f)).collect();
            out.push(msg("userFills", json!({"isSnapshot": true, "user": fmt::addr(user), "fills": fills})));
        }
        SubKey::UserFundings { user } => {
            let v = s.index.read().user_funding(user, 0, u64::MAX);
            let f: Vec<Value> = v.iter().map(|f| render::funding_ws(coins, f)).collect();
            out.push(msg("userFundings", json!({"isSnapshot": true, "user": fmt::addr(user), "fundings": f})));
        }
        SubKey::UserNonFundingLedgerUpdates { user } => {
            let v = s.index.read().user_ledger(user, 0, u64::MAX);
            let l: Vec<Value> = v.iter().map(|l| render::ledger(coins, l)).collect();
            out.push(msg(
                "userNonFundingLedgerUpdates",
                json!({"isSnapshot": true, "user": fmt::addr(user), "nonFundingLedgerUpdates": l}),
            ));
        }
        SubKey::WebData2 { user } => out.push(msg("webData2", info::web_data2(s, user, now))),
        SubKey::ActiveAssetCtx { coin } => match coins.resolve(coin) {
            Some(CoinRef::Perp(a)) => out.push(msg("activeAssetCtx", json!({"coin": coin, "ctx": info::asset_ctx_one(s, a, now)}))),
            Some(CoinRef::Spot(p)) => out.push(msg("activeSpotAssetCtx", json!({"coin": coin, "ctx": crate::spot::ctx(s, p, now)}))),
            None => {}
        },
        SubKey::ActiveAssetData { user, coin } => {
            if let Some(v) = info::active_asset_data(s, user, coin) {
                out.push(msg("activeAssetData", v));
            }
        }
        SubKey::ClearinghouseState { user, dex } => out.push(msg("clearinghouseState", clearinghouse_data(s, user, dex, now))),
        SubKey::OpenOrders { user, dex } => out.push(msg("openOrders", open_orders_data(s, user, dex))),
        SubKey::OrderUpdates { .. } | SubKey::UserEvents { .. } | SubKey::Notification { .. } => {}
    }
    out
}

fn error_msg(text: String) -> String {
    msg("error", Value::String(text))
}

/// Serve one upgraded connection until it closes.
pub async fn serve_conn(s: Arc<Shared>, socket: WebSocket, ip: String) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Utf8Bytes>(s.cfg.ws_buffer.max(16));
    let conn = Arc::new(Conn {
        id: s.hub.next_id.fetch_add(1, Ordering::Relaxed) + 1,
        tx,
        dead: AtomicBool::new(false),
        cancel: CancellationToken::new(),
    });
    let cancel = conn.cancel.clone();
    let writer = tokio::spawn(async move {
        if sink.send(Message::Text("Websocket connection established.".into())).await.is_err() {
            return;
        }
        loop {
            tokio::select! {
                m = rx.recv() => match m {
                    Some(m) => {
                        if sink.send(Message::Text(m)).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                _ = cancel.cancelled() => break,
            }
        }
        let _ = sink.close().await;
    });

    let mut my_subs: HashMap<SubKey, Value> = HashMap::new();
    loop {
        let next = tokio::select! {
            m = stream.next() => m,
            _ = conn.cancel.cancelled() => None,
        };
        let Some(Ok(m)) = next else { break };
        match m {
            Message::Text(t) => handle_text(&s, &conn, &mut my_subs, t.as_str(), &ip),
            Message::Close(_) => break,
            _ => {}
        }
        if conn.is_dead() {
            break;
        }
    }
    for k in my_subs.keys() {
        s.hub.remove(k, conn.id);
    }
    conn.kill();
    let _ = writer.await;
}

fn handle_text(s: &Arc<Shared>, conn: &Arc<Conn>, my_subs: &mut HashMap<SubKey, Value>, text: &str, ip: &str) {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            conn.send_str(error_msg(format!("Error parsing JSON into valid websocket request: {text}")));
            return;
        }
    };
    let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
    match method {
        "ping" => {
            conn.send_str(json!({"channel": "pong"}).to_string());
        }
        "subscribe" => {
            let sub = v.get("subscription").cloned().unwrap_or(Value::Null);
            let Some(key) = parse_sub(s, &sub) else {
                conn.send_str(error_msg(format!("Invalid subscription {sub}")));
                return;
            };
            if my_subs.contains_key(&key) {
                conn.send_str(error_msg(format!("Already subscribed: {sub}")));
                return;
            }
            if my_subs.len() >= s.cfg.ws_max_subs {
                conn.send_str(error_msg(format!("Cannot subscribe to more than {} channels: {sub}", s.cfg.ws_max_subs)));
                return;
            }
            conn.send_str(msg("subscriptionResponse", json!({"method": "subscribe", "subscription": sub})));
            {
                let _g = s.hub.publish_lock.lock();
                for m in snapshot_msgs(s, &key) {
                    conn.send_str(m);
                }
                s.hub.add(key.clone(), conn.clone());
            }
            my_subs.insert(key, sub);
        }
        "unsubscribe" => {
            let sub = v.get("subscription").cloned().unwrap_or(Value::Null);
            let key = parse_sub(s, &sub);
            match key.filter(|k| my_subs.contains_key(k)) {
                Some(k) => {
                    my_subs.remove(&k);
                    s.hub.remove(&k, conn.id);
                    conn.send_str(msg("subscriptionResponse", json!({"method": "unsubscribe", "subscription": sub})));
                }
                None => {
                    conn.send_str(error_msg(format!("Already unsubscribed: {sub}")));
                }
            }
        }
        "post" => {
            let id = v.get("id").cloned().unwrap_or(Value::Null);
            let req = v.get("request").cloned().unwrap_or(Value::Null);
            let rtype = req.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let payload = req.get("payload").cloned().unwrap_or(Value::Null);
            let post = |response: Value| msg("post", json!({"id": id, "response": response}));
            match rtype.as_str() {
                "info" => {
                    let it = payload.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string();
                    let r = match info::handle(s, &payload) {
                        Ok(data) => json!({"type": "info", "payload": {"type": it, "data": data}}),
                        Err(e) => json!({"type": "error", "payload": e}),
                    };
                    conn.send_str(post(r));
                }
                "action" => {
                    let s = s.clone();
                    let conn = conn.clone();
                    let ip = ip.to_string();
                    tokio::spawn(async move {
                        let r = s.exchange(&payload, ip).await;
                        conn.send_str(format!(
                            r#"{{"channel":"post","data":{{"id":{id},"response":{{"type":"action","payload":{r}}}}}}}"#
                        ));
                    });
                }
                _ => {
                    conn.send_str(post(json!({"type": "error", "payload": format!("Invalid post request type: {rtype}")})));
                }
            }
        }
        _ => {
            conn.send_str(error_msg(format!("Error parsing JSON into valid websocket request: {text}")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_consumer_is_killed_when_queue_full() {
        let (tx, _rx) = mpsc::channel(2);
        let c = Conn { id: 1, tx, dead: AtomicBool::new(false), cancel: CancellationToken::new() };
        assert!(c.send_str("a".into()));
        assert!(c.send_str("b".into()));
        assert!(!c.is_dead());
        assert!(!c.send_str("c".into())); // queue full -> dropped
        assert!(c.is_dead());
        assert!(c.cancel.is_cancelled());
        assert!(!c.send_str("d".into()));
    }

    #[test]
    fn closed_receiver_kills_conn() {
        let (tx, rx) = mpsc::channel(2);
        drop(rx);
        let c = Conn { id: 1, tx, dead: AtomicBool::new(false), cancel: CancellationToken::new() };
        assert!(!c.send_str("a".into()));
        assert!(c.is_dead());
    }
}
