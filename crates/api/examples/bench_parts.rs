//! Per-request CPU cost of the server-side /exchange pipeline pieces.
use interfaces::*;
use serde_json::{json, Value};
use std::time::Instant;

fn main() {
    let orders: Vec<_> = (0..50)
        .map(|j| json!({"a": 0, "b": true, "p": format!("{}", 1000 + j), "s": "0.01", "r": false, "t": {"limit": {"tif": "Gtc"}}}))
        .collect();
    let body = json!({"action": {"type": "order", "orders": orders, "grouping": "na"}, "nonce": 1, "signature": {"r": "0x01", "s": "0x02", "v": 27}, "vaultAddress": null}).to_string();
    let n = 20_000;
    let t = Instant::now();
    let mut v = Value::Null;
    for _ in 0..n {
        v = serde_json::from_str(&body).unwrap();
    }
    println!("json parse      {:6.1} us ({} bytes)", t.elapsed().as_secs_f64() * 1e6 / n as f64, body.len());
    let t = Instant::now();
    let mut u = None;
    for _ in 0..n {
        u = Some(api::wire::parse_exchange_request(&v).unwrap());
    }
    println!("hlwire parse    {:6.1} us", t.elapsed().as_secs_f64() * 1e6 / n as f64);
    let t = Instant::now();
    for _ in 0..n {
        u = Some(api::wire::parse_exchange_bytes(body.as_bytes()).unwrap());
    }
    println!("bytes parse     {:6.1} us (hlwire::parse_exchange_request_bytes; replaces json parse + hlwire parse)", t.elapsed().as_secs_f64() * 1e6 / n as f64);
    let tx = Tx::User(u.unwrap());
    let t = Instant::now();
    let mut h = Hash32::ZERO;
    for _ in 0..n {
        h = tx.hash();
    }
    println!("tx hash         {:6.1} us", t.elapsed().as_secs_f64() * 1e6 / n as f64);
    let w = api::shared::Waiters::new(100_000);
    let t = Instant::now();
    for i in 0..n {
        let mut hh = h;
        hh.0[0..8].copy_from_slice(&(i as u64).to_le_bytes());
        let rx = w.register(hh);
        w.resolve(&[TxReceipt { tx_hash: hh, user: Address::ZERO, result: Ok(ActionResponse::Default) }]);
        drop(rx);
    }
    println!("waiter reg+res  {:6.1} us", t.elapsed().as_secs_f64() * 1e6 / n as f64);
    let statuses: Vec<_> = (0..50).map(|i| OrderStatusResp::Resting { oid: i, cloid: None }).collect();
    let r: Result<ActionResponse, String> = Ok(ActionResponse::Order(statuses));
    let t = Instant::now();
    let mut s = String::new();
    for _ in 0..n {
        s = api::render::action_result(&r).to_string();
    }
    println!("render (Value)  {:6.1} us ({} bytes)", t.elapsed().as_secs_f64() * 1e6 / n as f64, s.len());
    let t = Instant::now();
    for _ in 0..n {
        s = api::render::action_result_string(&r);
    }
    println!("render (direct) {:6.1} us ({} bytes)", t.elapsed().as_secs_f64() * 1e6 / n as f64, s.len());
}
