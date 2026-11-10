//! Spot views (D25): PURR/USDC (pair 0, asset 10000) has a real order book;
//! other listed pairs (the "@1" alias) carry nominal data.

use crate::coins::Coins;
use crate::fmt;
use crate::shared::Shared;
use interfaces::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Hold of one resting spot order, identical to `state::spot::order_hold`
/// (tested against it in tests/spot.rs): buy = `n + ceil(n * max(taker,
/// maker, 0))` quote with `n = ceil(px * sz)`, sell = `sz` base.
pub fn order_hold(params: &ChainParams, o: &RestingOrderView) -> (bool, Usd) {
    match o.side {
        Side::Bid => {
            let rate = params.taker_fee.max(params.maker_fee).max(Rate::ZERO);
            let n = notional(o.limit_px, o.sz, Rounding::Ceil);
            (true, n + usd_mul_rate(n, rate, Rounding::Ceil))
        }
        Side::Ask => (false, Usd::from_raw(o.sz.raw() / (SZ_SCALE / USD_SCALE))),
    }
}

/// Per-token holds (token index -> amount) from a user's open orders.
pub fn holds(coins: &Coins, params: &ChainParams, orders: &[RestingOrderView]) -> BTreeMap<u32, Usd> {
    let mut m: BTreeMap<u32, Usd> = BTreeMap::new();
    for o in orders {
        let Some(p) = coins.spot_pair_of_asset(o.asset) else { continue };
        if o.is_trigger {
            continue;
        }
        let (is_quote, amt) = order_hold(params, o);
        *m.entry(if is_quote { p.quote } else { p.base }).or_default() += amt;
    }
    m
}

/// spotClearinghouseState for `user`.
pub fn clearinghouse(s: &Shared, user: &Address) -> Value {
    let (a, orders) = s.with_state(|q| (q.account(user), q.open_orders(user)));
    clearinghouse_from(&s.coins, &s.genesis.params, &a, &orders)
}

pub fn clearinghouse_from(coins: &Coins, params: &ChainParams, a: &AccountView, orders: &[RestingOrderView]) -> Value {
    let h = holds(coins, params, orders);
    let balances: Vec<Value> = a
        .spot_balances
        .iter()
        .filter(|(_, amt)| !amt.is_zero())
        .map(|(tok, amt)| {
            let name = coins.token(*tok).map(|t| t.name.clone()).unwrap_or_else(|| format!("?{tok}"));
            let hold = h.get(tok).copied().unwrap_or(Usd::ZERO);
            json!({"coin": name, "token": tok, "total": fmt::usd(*amt), "hold": fmt::usd(hold), "entryNtl": "0.0"})
        })
        .collect();
    json!({"balances": balances})
}

/// Total genesis supply of a token (spot balances; no minting on this chain).
fn token_supply(g: &Genesis, token: u32) -> Usd {
    g.spot_balances.iter().filter(|(_, t, _)| *t == token).fold(Usd::ZERO, |acc, (_, _, a)| acc + *a)
}

/// Spot asset ctx for pair `pair`: markPx / midPx from the chain's spot
/// book, dayNtlVlm / dayBaseVlm / prevDayPx from indexed fills.
pub fn ctx(s: &Shared, pair: u32, now: u64) -> Value {
    let coins = &s.coins;
    let Some(p) = coins.spot_pair(pair) else { return Value::Null };
    let supply = fmt::usd(token_supply(&s.genesis, p.base));
    let Some(asset) = coins.tradable_spot_asset(pair) else {
        return json!({
            "prevDayPx": "1.0",
            "dayNtlVlm": "0.0",
            "markPx": "1.0",
            "midPx": Value::Null,
            "circulatingSupply": supply,
            "coin": p.name,
            "totalSupply": supply,
            "dayBaseVlm": "0.0",
        });
    };
    let c = s.with_state(|q| q.market_ctx(asset));
    let idx = s.index.read();
    let prev = idx.prev_day_px(asset, now).unwrap_or(c.mark_px);
    json!({
        "prevDayPx": fmt::px(prev),
        "dayNtlVlm": fmt::dec(idx.day_ntl_vlm(asset, now).max(0), USD_DECIMALS),
        "markPx": fmt::px(c.mark_px),
        "midPx": c.mid_px.map(|x| Value::from(fmt::px(x))).unwrap_or(Value::Null),
        "circulatingSupply": supply,
        "coin": p.name,
        "totalSupply": supply,
        "dayBaseVlm": fmt::dec(idx.day_base_vlm(asset, now).max(0), SZ_DECIMALS),
    })
}

pub fn ctxs(s: &Shared, now: u64) -> Vec<Value> {
    s.coins.spot_pairs.iter().map(|p| ctx(s, p.index, now)).collect()
}

/// allMids entries for tradable spot pairs: book mid, else spot mark.
pub fn mids(s: &Shared) -> Vec<(String, Px)> {
    let coins = &s.coins;
    let tradable: Vec<(String, AssetId)> = coins
        .spot_pairs
        .iter()
        .filter_map(|p| coins.tradable_spot_asset(p.index).map(|a| (p.name.clone(), a)))
        .collect();
    s.with_state(|q| tradable.into_iter().map(|(n, a)| (n, q.mid(a).unwrap_or_else(|| q.market_ctx(a).mark_px))).collect())
}
