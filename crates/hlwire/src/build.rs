//! Action JSON builders producing the same key order as the Python SDK
//! (`hyperliquid/exchange.py`, `utils/signing.py`). Used by the simulator and
//! tests to create SDK-identical requests from typed values.

use crate::decode::cloid_to_hex;
use interfaces::*;
use serde_json::{Map, Value};

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Map::with_capacity(pairs.len());
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    Value::Object(m)
}

fn s(x: impl Into<String>) -> Value {
    Value::String(x.into())
}

/// OrderWire `{a, b, p, s, r, t, c?}`; `t` is `{limit:{tif}}` or
/// `{trigger:{isMarket, triggerPx, tpsl}}`. Prices/sizes use the minimal
/// decimal form (Python `float_to_wire`). No precision checks: the simulator
/// may deliberately send invalid orders (see [`order_to_wire_json_checked`]).
pub fn order_to_wire_json(o: &OrderRequest) -> Value {
    let t = match o.kind {
        OrderKind::Limit { tif } => {
            let tif = match tif {
                Tif::Gtc => "Gtc",
                Tif::Ioc => "Ioc",
                Tif::Alo => "Alo",
            };
            obj(vec![("limit", obj(vec![("tif", s(tif))]))])
        }
        OrderKind::Trigger { trigger_px, is_market, tpsl } => obj(vec![(
            "trigger",
            obj(vec![
                ("isMarket", Value::Bool(is_market)),
                ("triggerPx", s(trigger_px.to_wire())),
                ("tpsl", s(if tpsl == TpSl::Tp { "tp" } else { "sl" })),
            ]),
        )]),
    };
    let mut v = vec![
        ("a", Value::from(o.asset)),
        ("b", Value::Bool(o.is_buy)),
        ("p", s(o.limit_px.to_wire())),
        ("s", s(o.sz.to_wire())),
        ("r", Value::Bool(o.reduce_only)),
        ("t", t),
    ];
    if let Some(c) = &o.cloid {
        v.push(("c", s(cloid_to_hex(c))));
    }
    obj(v)
}

/// Like [`order_to_wire_json`] but rejects sizes with more than
/// `sz_decimals` decimals and prices violating the HL price rule
/// (<= 5 significant figures unless integer, <= 6 - sz_decimals decimals).
pub fn order_to_wire_json_checked(o: &OrderRequest, sz_decimals: u32) -> Result<Value, String> {
    if !o.sz.has_at_most_decimals(sz_decimals) {
        return Err(format!("size {} has more than {sz_decimals} decimals", o.sz));
    }
    let spec = MarketSpec {
        asset: o.asset,
        name: String::new(),
        sz_decimals,
        max_leverage: 1,
        only_isolated: false,
        initial_px: Px(1),
    };
    if !spec.is_valid_px(o.limit_px) {
        return Err(format!("invalid price {}", o.limit_px));
    }
    if let OrderKind::Trigger { trigger_px, .. } = o.kind {
        if !spec.is_valid_px(trigger_px) {
            return Err(format!("invalid trigger price {trigger_px}"));
        }
    }
    Ok(order_to_wire_json(o))
}

pub fn grouping_str(g: Grouping) -> &'static str {
    match g {
        Grouping::Na => "na",
        Grouping::NormalTpsl => "normalTpsl",
        Grouping::PositionTpsl => "positionTpsl",
    }
}

/// `{type:"order", orders:[...], grouping}`.
pub fn order_action(orders: &[OrderRequest], grouping: Grouping) -> Value {
    obj(vec![
        ("type", s("order")),
        ("orders", Value::Array(orders.iter().map(order_to_wire_json).collect())),
        ("grouping", s(grouping_str(grouping))),
    ])
}

/// `{type:"cancel", cancels:[{a, o}]}`.
pub fn cancel_action(cancels: &[(AssetId, Oid)]) -> Value {
    obj(vec![
        ("type", s("cancel")),
        (
            "cancels",
            Value::Array(cancels.iter().map(|(a, o)| obj(vec![("a", Value::from(*a)), ("o", Value::from(*o))])).collect()),
        ),
    ])
}

/// `{type:"cancelByCloid", cancels:[{asset, cloid}]}`.
pub fn cancel_by_cloid_action(cancels: &[(AssetId, Cloid)]) -> Value {
    obj(vec![
        ("type", s("cancelByCloid")),
        (
            "cancels",
            Value::Array(
                cancels.iter().map(|(a, c)| obj(vec![("asset", Value::from(*a)), ("cloid", s(cloid_to_hex(c)))])).collect(),
            ),
        ),
    ])
}

fn oid_json(o: &OidOrCloid) -> Value {
    match o {
        OidOrCloid::Oid(x) => Value::from(*x),
        OidOrCloid::Cloid(c) => s(cloid_to_hex(c)),
    }
}

/// `{type:"modify", oid, order}` (TS SDK `modify`; Python only sends batchModify).
pub fn modify_action(oid: &OidOrCloid, order: &OrderRequest) -> Value {
    obj(vec![("type", s("modify")), ("oid", oid_json(oid)), ("order", order_to_wire_json(order))])
}

/// `{type:"batchModify", modifies:[{oid, order}]}`.
pub fn batch_modify_action(modifies: &[(OidOrCloid, OrderRequest)]) -> Value {
    obj(vec![
        ("type", s("batchModify")),
        (
            "modifies",
            Value::Array(
                modifies.iter().map(|(o, ord)| obj(vec![("oid", oid_json(o)), ("order", order_to_wire_json(ord))])).collect(),
            ),
        ),
    ])
}

/// `{type:"scheduleCancel", time?}`.
pub fn schedule_cancel_action(time: Option<u64>) -> Value {
    let mut v = vec![("type", s("scheduleCancel"))];
    if let Some(t) = time {
        v.push(("time", Value::from(t)));
    }
    obj(v)
}

/// `{type:"updateLeverage", asset, isCross, leverage}`.
pub fn update_leverage_action(asset: AssetId, is_cross: bool, leverage: u32) -> Value {
    obj(vec![
        ("type", s("updateLeverage")),
        ("asset", Value::from(asset)),
        ("isCross", Value::Bool(is_cross)),
        ("leverage", Value::from(leverage)),
    ])
}

/// `{type:"updateIsolatedMargin", asset, isBuy, ntli}`; `ntli` is the raw
/// 1e-6 USD integer (Python `float_to_usd_int`).
pub fn update_isolated_margin_action(asset: AssetId, is_buy: bool, ntli: Usd) -> Value {
    obj(vec![
        ("type", s("updateIsolatedMargin")),
        ("asset", Value::from(asset)),
        ("isBuy", Value::Bool(is_buy)),
        ("ntli", Value::from(ntli.0)),
    ])
}

/// `{type:"setReferrer", code}`.
pub fn set_referrer_action(code: &str) -> Value {
    obj(vec![("type", s("setReferrer")), ("code", s(code))])
}

/// `{type:"noop"}`.
pub fn noop_action() -> Value {
    obj(vec![("type", s("noop"))])
}

/// `{destination, amount, time, type:"usdSend"}` (unsigned; use
/// `sign_user_signed_action` to add signatureChainId/hyperliquidChain).
pub fn usd_send_action(destination: &Address, amount: Usd, time: u64) -> Value {
    obj(vec![
        ("destination", s(destination.to_hex())),
        ("amount", s(amount.to_wire())),
        ("time", Value::from(time)),
        ("type", s("usdSend")),
    ])
}

/// `{destination, amount, token, time, type:"spotSend"}`.
pub fn spot_send_action(destination: &Address, token: &str, amount: Usd, time: u64) -> Value {
    obj(vec![
        ("destination", s(destination.to_hex())),
        ("amount", s(amount.to_wire())),
        ("token", s(token)),
        ("time", Value::from(time)),
        ("type", s("spotSend")),
    ])
}

/// `{destination, amount, time, type:"withdraw3"}`.
pub fn withdraw3_action(destination: &Address, amount: Usd, time: u64) -> Value {
    obj(vec![
        ("destination", s(destination.to_hex())),
        ("amount", s(amount.to_wire())),
        ("time", Value::from(time)),
        ("type", s("withdraw3")),
    ])
}

/// `{type:"usdClassTransfer", amount, toPerp, nonce}`.
pub fn usd_class_transfer_action(amount: Usd, to_perp: bool, nonce: u64) -> Value {
    obj(vec![
        ("type", s("usdClassTransfer")),
        ("amount", s(amount.to_wire())),
        ("toPerp", Value::Bool(to_perp)),
        ("nonce", Value::from(nonce)),
    ])
}

/// `{type:"approveAgent", agentAddress, agentName?, nonce}`. With `name =
/// None` the key is omitted (Python SDK; signed as "").
pub fn approve_agent_action(agent: &Address, name: Option<&str>, nonce: u64) -> Value {
    let mut v = vec![("type", s("approveAgent")), ("agentAddress", s(agent.to_hex()))];
    if let Some(n) = name {
        v.push(("agentName", s(n)));
    }
    v.push(("nonce", Value::from(nonce)));
    obj(v)
}

/// Typed [`Action`] -> SDK-shaped (Python key order) unsigned action JSON.
pub fn action_to_json(a: &Action) -> Value {
    match a {
        Action::Order { orders, grouping } => order_action(orders, *grouping),
        Action::Cancel { cancels } => cancel_action(cancels),
        Action::CancelByCloid { cancels } => cancel_by_cloid_action(cancels),
        Action::Modify { oid, order } => modify_action(oid, order),
        Action::BatchModify { modifies } => batch_modify_action(modifies),
        Action::ScheduleCancel { time } => schedule_cancel_action(*time),
        Action::UpdateLeverage { asset, is_cross, leverage } => update_leverage_action(*asset, *is_cross, *leverage),
        Action::UpdateIsolatedMargin { asset, is_buy, ntli } => update_isolated_margin_action(*asset, *is_buy, *ntli),
        Action::UsdSend { destination, amount, time } => usd_send_action(destination, *amount, *time),
        Action::SpotSend { destination, token, amount, time } => spot_send_action(destination, token, *amount, *time),
        Action::UsdClassTransfer { amount, to_perp, nonce } => usd_class_transfer_action(*amount, *to_perp, *nonce),
        Action::Withdraw3 { destination, amount, time } => withdraw3_action(destination, *amount, *time),
        Action::ApproveAgent { agent_address, agent_name, nonce } => {
            approve_agent_action(agent_address, agent_name.as_deref(), *nonce)
        }
        Action::SetReferrer { code } => set_referrer_action(code),
        Action::Noop => noop_action(),
    }
}
