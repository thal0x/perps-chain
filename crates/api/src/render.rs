//! Interface types -> HL JSON shapes.

use crate::coins::Coins;
use crate::fmt;
use crate::indexer::*;
use interfaces::*;
use serde_json::{json, Map, Value};

pub const ZERO_HASH: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

pub fn side(s: Side) -> &'static str {
    s.hl_str()
}

/// HL fill direction for perps.
pub fn dir(side: Side, start: Sz, sz: Sz) -> &'static str {
    let (s, q) = (start.raw(), sz.raw().abs());
    match side {
        Side::Bid => {
            if s >= 0 {
                "Open Long"
            } else if q > -s {
                "Short > Long"
            } else {
                "Close Short"
            }
        }
        Side::Ask => {
            if s <= 0 {
                "Open Short"
            } else if q > s {
                "Long > Short"
            } else {
                "Close Long"
            }
        }
    }
}

pub fn fill(coins: &Coins, f: &FillRec) -> Value {
    let mut m = Map::new();
    m.insert("coin".into(), coins.name(f.asset).into());
    m.insert("px".into(), fmt::px(f.px).into());
    m.insert("sz".into(), fmt::sz_abs(f.sz).into());
    m.insert("side".into(), side(f.side).into());
    m.insert("time".into(), f.time.into());
    m.insert("startPosition".into(), fmt::sz(f.start_pos).into());
    m.insert("dir".into(), dir(f.side, f.start_pos, f.sz).into());
    m.insert("closedPnl".into(), fmt::usd(f.closed_pnl).into());
    m.insert("hash".into(), fmt::hash(&f.hash).into());
    m.insert("oid".into(), f.oid.into());
    m.insert("crossed".into(), f.crossed.into());
    m.insert("fee".into(), fmt::usd(f.fee).into());
    m.insert("tid".into(), f.tid.into());
    if let Some(c) = &f.cloid {
        m.insert("cloid".into(), fmt::cloid(c).into());
    }
    if let Some(l) = &f.liquidation {
        let method = match l.method {
            LiquidationMethod::Market => "market",
            LiquidationMethod::Backstop | LiquidationMethod::Adl => "backstop",
        };
        m.insert(
            "liquidation".into(),
            json!({"liquidatedUser": fmt::addr(&l.liquidated_user), "markPx": fmt::px(l.mark_px), "method": method}),
        );
    }
    m.insert("feeToken".into(), "USDC".into());
    m.insert("twapId".into(), Value::Null);
    Value::Object(m)
}

/// HL `aggregateByTime`: merge partial fills of the same order at the same time.
pub fn aggregate_fills(fills: Vec<FillRec>) -> Vec<FillRec> {
    let mut out: Vec<(FillRec, i128)> = Vec::new(); // (rec, px*sz accumulator)
    for f in fills {
        if let Some((last, acc)) = out.last_mut() {
            if last.oid == f.oid && last.time == f.time && last.side == f.side && last.asset == f.asset {
                *acc += f.px.raw() * f.sz.raw();
                let tot = last.sz.raw() + f.sz.raw();
                last.sz = Sz::from_raw(tot);
                last.px = Px::from_raw(if tot > 0 { *acc / tot } else { last.px.raw() });
                last.closed_pnl = last.closed_pnl + f.closed_pnl;
                last.fee = last.fee + f.fee;
                continue;
            }
        }
        let acc = f.px.raw() * f.sz.raw();
        out.push((f, acc));
    }
    out.into_iter().map(|(f, _)| f).collect()
}

pub fn status_str(s: OrderLifecycle) -> &'static str {
    match s {
        OrderLifecycle::Open => "open",
        OrderLifecycle::Filled => "filled",
        OrderLifecycle::Canceled => "canceled",
        OrderLifecycle::Triggered => "triggered",
        OrderLifecycle::Rejected => "rejected",
        OrderLifecycle::MarginCanceled => "marginCanceled",
        OrderLifecycle::ReduceOnlyCanceled => "reduceOnlyCanceled",
        OrderLifecycle::SelfTradeCanceled => "selfTradeCanceled",
        OrderLifecycle::ScheduledCancel => "scheduledCancel",
        OrderLifecycle::LiquidatedCanceled => "liquidatedCanceled",
    }
}

/// Canceled by the system rather than the user (userEvents nonUserCancel).
pub fn is_non_user_cancel(s: OrderLifecycle) -> bool {
    matches!(
        s,
        OrderLifecycle::MarginCanceled
            | OrderLifecycle::ReduceOnlyCanceled
            | OrderLifecycle::SelfTradeCanceled
            | OrderLifecycle::ScheduledCancel
            | OrderLifecycle::LiquidatedCanceled
    )
}

pub fn order_type_str(o: &RestingOrderView) -> &'static str {
    if let Some((_, is_market, tpsl)) = o.trigger {
        return match (tpsl, is_market) {
            (TpSl::Sl, true) => "Stop Market",
            (TpSl::Sl, false) => "Stop Limit",
            (TpSl::Tp, true) => "Take Profit Market",
            (TpSl::Tp, false) => "Take Profit Limit",
        };
    }
    match o.order_type.as_str() {
        "Market" | "market" => "Market",
        _ => "Limit",
    }
}

pub fn trigger_condition(o: &RestingOrderView) -> String {
    match o.trigger {
        Some((px, _, tpsl)) => {
            let above = matches!((tpsl, o.side), (TpSl::Tp, Side::Ask) | (TpSl::Sl, Side::Bid));
            format!("Price {} {}", if above { "above" } else { "below" }, fmt::px(px))
        }
        None => "N/A".into(),
    }
}

fn tif_str(t: Tif) -> &'static str {
    match t {
        Tif::Gtc => "Gtc",
        Tif::Ioc => "Ioc",
        Tif::Alo => "Alo",
    }
}

/// HL `FrontendOpenOrder`.
pub fn frontend_order(coins: &Coins, o: &RestingOrderView) -> Value {
    let is_trigger = o.is_trigger || o.trigger.is_some();
    json!({
        "coin": coins.name(o.asset),
        "side": side(o.side),
        "limitPx": fmt::px(o.limit_px),
        "sz": fmt::sz_abs(o.sz),
        "oid": o.oid,
        "timestamp": o.timestamp,
        "origSz": fmt::sz_abs(if o.orig_sz.is_zero() { o.sz } else { o.orig_sz }),
        "triggerCondition": trigger_condition(o),
        "isTrigger": is_trigger,
        "triggerPx": o.trigger.map(|t| fmt::px(t.0)).unwrap_or_else(|| "0.0".into()),
        "children": [],
        "isPositionTpsl": false,
        "reduceOnly": o.reduce_only,
        "orderType": order_type_str(o),
        "tif": if is_trigger { Value::Null } else { o.tif.map(|t| Value::from(tif_str(t))).unwrap_or(Value::Null) },
        "cloid": o.cloid.map(|c| Value::from(fmt::cloid(&c))).unwrap_or(Value::Null),
    })
}

/// HL `OpenOrder` (openOrders / orderUpdates.order).
pub fn open_order(coins: &Coins, o: &RestingOrderView) -> Value {
    let mut m = Map::new();
    m.insert("coin".into(), coins.name(o.asset).into());
    m.insert("side".into(), side(o.side).into());
    m.insert("limitPx".into(), fmt::px(o.limit_px).into());
    m.insert("sz".into(), fmt::sz_abs(o.sz).into());
    m.insert("oid".into(), o.oid.into());
    m.insert("timestamp".into(), o.timestamp.into());
    m.insert("origSz".into(), fmt::sz_abs(if o.orig_sz.is_zero() { o.sz } else { o.orig_sz }).into());
    if let Some(c) = &o.cloid {
        m.insert("cloid".into(), fmt::cloid(c).into());
    }
    if o.reduce_only {
        m.insert("reduceOnly".into(), true.into());
    }
    Value::Object(m)
}

pub fn order_status_entry(coins: &Coins, r: &OrderRec) -> Value {
    json!({"order": frontend_order(coins, &r.order), "status": status_str(r.status), "statusTimestamp": r.status_ts})
}

pub fn order_update(coins: &Coins, r: &OrderRec) -> Value {
    json!({"order": open_order(coins, &r.order), "status": status_str(r.status), "statusTimestamp": r.status_ts})
}

pub fn funding_delta(coins: &Coins, f: &FundingRec) -> Value {
    json!({
        "type": "funding",
        "coin": coins.name(f.asset),
        "usdc": fmt::usd(f.usdc),
        "szi": fmt::sz(f.szi),
        "fundingRate": fmt::rate(f.rate),
        "nSamples": Value::Null,
    })
}

pub fn user_funding(coins: &Coins, f: &FundingRec) -> Value {
    json!({"time": f.time, "hash": ZERO_HASH, "delta": funding_delta(coins, f)})
}

/// WS userFundings / userEvents funding entry.
pub fn funding_ws(coins: &Coins, f: &FundingRec) -> Value {
    json!({
        "time": f.time,
        "coin": coins.name(f.asset),
        "usdc": fmt::usd(f.usdc),
        "szi": fmt::sz(f.szi),
        "fundingRate": fmt::rate(f.rate),
        "nSamples": Value::Null,
    })
}

pub fn ledger_delta(coins: &Coins, d: &LedgerDelta) -> Value {
    match d {
        LedgerDelta::Deposit { usdc } => json!({"type": "deposit", "usdc": fmt::usd_abs(*usdc)}),
        LedgerDelta::Withdraw { usdc, nonce, fee } => {
            json!({"type": "withdraw", "usdc": fmt::usd_abs(*usdc), "nonce": nonce, "fee": fmt::usd_abs(*fee)})
        }
        LedgerDelta::InternalTransfer { usdc, user, destination, fee } => json!({
            "type": "internalTransfer",
            "usdc": fmt::usd_abs(*usdc),
            "user": fmt::addr(user),
            "destination": fmt::addr(destination),
            "fee": fmt::usd_abs(*fee),
        }),
        LedgerDelta::SpotTransfer { token, amount, user, destination, fee } => {
            // `token` may be "NAME:0x..", "NAME" or a numeric index.
            let name = token.split(':').next().unwrap_or(token).to_string();
            let name = match name.parse::<u32>() {
                Ok(i) => coins.token(i).map(|t| t.name.clone()).unwrap_or(name),
                Err(_) => name,
            };
            let usdc_value = if name == "USDC" { fmt::usd_abs(*amount) } else { "0.0".into() };
            json!({
                "type": "spotTransfer",
                "token": name,
                "amount": fmt::usd_abs(*amount),
                "usdcValue": usdc_value,
                "user": fmt::addr(user),
                "destination": fmt::addr(destination),
                "fee": fmt::usd_abs(*fee),
                "nativeTokenFee": "0.0",
                "nonce": Value::Null,
                "feeToken": "USDC",
            })
        }
        LedgerDelta::AccountClassTransfer { usdc, to_perp } => {
            json!({"type": "accountClassTransfer", "usdc": fmt::usd_abs(*usdc), "toPerp": to_perp})
        }
        LedgerDelta::Liquidation { account_value, liquidated_ntl_pos } => json!({
            "type": "liquidation",
            "liquidatedNtlPos": fmt::usd_abs(*liquidated_ntl_pos),
            "accountValue": fmt::usd(*account_value),
            "leverageType": "Cross",
            "liquidatedPositions": [],
        }),
    }
}

pub fn ledger(coins: &Coins, l: &LedgerRec) -> Value {
    json!({"time": l.time, "hash": fmt::hash(&l.hash), "delta": ledger_delta(coins, &l.delta)})
}

pub fn trade(coins: &Coins, t: &TradeRec) -> Value {
    json!({
        "coin": coins.name(t.asset),
        "side": side(t.side),
        "px": fmt::px(t.px),
        "sz": fmt::sz_abs(t.sz),
        "time": t.time,
        "hash": fmt::hash(&t.hash),
        "tid": t.tid,
        "users": [fmt::addr(&t.buyer), fmt::addr(&t.seller)],
    })
}

pub fn candle(coin: &str, iv: crate::candle::Interval, c: &Candle) -> Value {
    json!({
        "t": c.t,
        "T": c.close_t,
        "s": coin,
        "i": iv.as_str(),
        "o": fmt::px(c.o),
        "c": fmt::px(c.c),
        "h": fmt::px(c.h),
        "l": fmt::px(c.l),
        "v": fmt::dec(c.v, SZ_DECIMALS),
        "n": c.n,
    })
}

// ------------------------------------------------------------ state views ---

pub fn meta(coins: &Coins) -> Value {
    let mut tables: Vec<u32> = coins.markets.iter().map(|m| m.max_leverage).collect();
    tables.sort_unstable();
    tables.dedup();
    let universe: Vec<Value> = coins
        .markets
        .iter()
        .map(|m| {
            let mut o = Map::new();
            o.insert("szDecimals".into(), m.sz_decimals.into());
            o.insert("name".into(), m.name.clone().into());
            o.insert("maxLeverage".into(), m.max_leverage.into());
            o.insert("marginTableId".into(), m.max_leverage.into());
            if m.only_isolated {
                o.insert("onlyIsolated".into(), true.into());
                o.insert("marginMode".into(), "strictIsolated".into());
            }
            Value::Object(o)
        })
        .collect();
    let margin_tables: Vec<Value> = tables
        .iter()
        .map(|l| json!([l, {"description": "", "marginTiers": [{"lowerBound": "0.0", "maxLeverage": l}]}]))
        .collect();
    json!({"universe": universe, "marginTables": margin_tables, "collateralToken": 0})
}

pub fn leverage(l: &Leverage, raw_usd: Usd) -> Value {
    match l {
        Leverage::Cross(v) => json!({"type": "cross", "value": v}),
        Leverage::Isolated(v) => json!({"type": "isolated", "value": v, "rawUsd": fmt::usd(raw_usd)}),
    }
}

pub fn asset_ctx(c: &MarketCtx, impact: Option<(Px, Px)>, day_base_vlm: i128) -> Value {
    json!({
        "prevDayPx": fmt::px(c.prev_day_px),
        "dayNtlVlm": fmt::usd_nonneg(c.day_ntl_vlm),
        "markPx": fmt::px(c.mark_px),
        "midPx": c.mid_px.map(|p| Value::from(fmt::px(p))).unwrap_or(Value::Null),
        "funding": fmt::rate(c.funding),
        "openInterest": fmt::sz_abs(c.open_interest),
        "premium": fmt::rate(c.premium),
        "oraclePx": fmt::px(c.oracle_px),
        "impactPxs": impact.map(|(b, a)| json!([fmt::px(b), fmt::px(a)])).unwrap_or(Value::Null),
        "dayBaseVlm": fmt::dec(day_base_vlm.max(0), SZ_DECIMALS),
    })
}

fn margin_summary(m: &MarginSummary) -> Value {
    json!({
        "accountValue": fmt::usd(m.account_value),
        "totalNtlPos": fmt::usd_abs(m.total_ntl_pos),
        "totalRawUsd": fmt::usd(m.total_raw_usd),
        "totalMarginUsed": fmt::usd_nonneg(m.total_margin_used),
    })
}

pub fn position(coins: &Coins, p: &PositionView) -> Value {
    json!({
        "type": "oneWay",
        "position": {
            "coin": coins.name(p.asset),
            "szi": fmt::sz(p.szi),
            "leverage": leverage(&p.leverage, p.margin_used),
            "entryPx": fmt::px(p.entry_px),
            "positionValue": fmt::usd_abs(p.position_value),
            "unrealizedPnl": fmt::usd(p.unrealized_pnl),
            "returnOnEquity": fmt::rate(p.return_on_equity),
            "liquidationPx": p.liquidation_px.map(|x| Value::from(fmt::px(x))).unwrap_or(Value::Null),
            "marginUsed": fmt::usd_nonneg(p.margin_used),
            "maxLeverage": p.max_leverage,
            "cumFunding": {
                "allTime": fmt::usd(p.cum_funding_all_time),
                "sinceOpen": fmt::usd(p.cum_funding_since_open),
                "sinceChange": fmt::usd(p.cum_funding_since_open),
            },
        },
    })
}

pub fn clearinghouse(coins: &Coins, a: &AccountView, time: u64) -> Value {
    let positions: Vec<Value> = a.positions.iter().filter(|p| !p.szi.is_zero()).map(|p| position(coins, p)).collect();
    json!({
        "marginSummary": margin_summary(&a.margin_summary),
        "crossMarginSummary": margin_summary(&a.cross_margin_summary),
        "crossMaintenanceMarginUsed": fmt::usd_nonneg(a.cross_maintenance_margin_used),
        "withdrawable": fmt::usd_nonneg(a.withdrawable),
        "assetPositions": positions,
        "time": time,
    })
}

pub fn spot_clearinghouse(coins: &Coins, a: &AccountView) -> Value {
    let balances: Vec<Value> = a
        .spot_balances
        .iter()
        .filter(|(_, amt)| !amt.is_zero())
        .map(|(tok, amt)| {
            let name = coins.token(*tok).map(|t| t.name.clone()).unwrap_or_else(|| format!("?{tok}"));
            json!({"coin": name, "token": tok, "total": fmt::usd(*amt), "hold": "0.0", "entryNtl": "0.0"})
        })
        .collect();
    json!({"balances": balances})
}

pub fn spot_meta(coins: &Coins) -> Value {
    let tokens: Vec<Value> = coins
        .spot_tokens
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "szDecimals": t.sz_decimals,
                "weiDecimals": t.wei_decimals,
                "index": t.index,
                "tokenId": format!("0x{}", hex::encode(t.token_id)),
                "isCanonical": true,
                "evmContract": Value::Null,
                "fullName": Value::Null,
                "deployerTradingFeeShare": "0.0",
            })
        })
        .collect();
    let universe: Vec<Value> = coins
        .spot_pairs
        .iter()
        .map(|p| json!({"tokens": [p.base, p.quote], "name": p.name, "index": p.index, "isCanonical": p.is_canonical}))
        .collect();
    json!({"universe": universe, "tokens": tokens})
}

/// Spot asset ctx (no spot books on this chain: prices are nominal).
pub fn spot_ctx(coins: &Coins, pair: u32) -> Value {
    let name = coins.spot_pair(pair).map(|p| p.name.clone()).unwrap_or_else(|| format!("@{pair}"));
    json!({
        "prevDayPx": "1.0",
        "dayNtlVlm": "0.0",
        "markPx": "1.0",
        "midPx": Value::Null,
        "circulatingSupply": "0.0",
        "coin": name,
        "totalSupply": "0.0",
        "dayBaseVlm": "0.0",
    })
}

pub fn l2_levels(levels: &[(Px, Sz, u32)]) -> Value {
    Value::Array(levels.iter().map(|(p, s, n)| json!({"px": fmt::px(*p), "sz": fmt::sz_abs(*s), "n": n})).collect())
}

/// Aggregate best-first levels to `n_sig` significant figures.
pub fn aggregate_levels(levels: &[(Px, Sz, u32)], n_sig: Option<u32>, mantissa: Option<u32>, is_ask: bool, max: usize) -> Vec<(Px, Sz, u32)> {
    let Some(n) = n_sig else {
        return levels.iter().take(max).copied().collect();
    };
    let mantissa = if n == 5 { mantissa } else { None };
    let mut out: Vec<(Px, Sz, u32)> = Vec::new();
    for (p, s, c) in levels {
        let rp = Px(fmt::round_sig(p.0, n, mantissa, is_ask));
        match out.last_mut() {
            Some(last) if last.0 == rp => {
                last.1 = last.1 + *s;
                last.2 += c;
            }
            _ => {
                if out.len() >= max {
                    break;
                }
                out.push((rp, *s, *c));
            }
        }
    }
    out
}

// -------------------------------------------------------------- exchange ---

pub fn order_status(s: &OrderStatusResp) -> Value {
    match s {
        OrderStatusResp::Resting { oid, cloid } => {
            let mut m = Map::new();
            m.insert("oid".into(), (*oid).into());
            if let Some(c) = cloid {
                m.insert("cloid".into(), fmt::cloid(c).into());
            }
            json!({ "resting": Value::Object(m) })
        }
        OrderStatusResp::Filled { total_sz, avg_px, oid, cloid } => {
            let mut m = Map::new();
            m.insert("totalSz".into(), fmt::sz_abs(*total_sz).into());
            m.insert("avgPx".into(), fmt::px(*avg_px).into());
            m.insert("oid".into(), (*oid).into());
            if let Some(c) = cloid {
                m.insert("cloid".into(), fmt::cloid(c).into());
            }
            json!({ "filled": Value::Object(m) })
        }
        OrderStatusResp::WaitingForTrigger { .. } => "waitingForTrigger".into(),
        OrderStatusResp::WaitingForFill { .. } => "waitingForFill".into(),
        OrderStatusResp::Error(e) => json!({ "error": e }),
    }
}

pub fn action_result(r: &Result<ActionResponse, String>) -> Value {
    match r {
        Ok(ActionResponse::Order(st)) => json!({
            "status": "ok",
            "response": {"type": "order", "data": {"statuses": st.iter().map(order_status).collect::<Vec<_>>()}},
        }),
        Ok(ActionResponse::Cancel(st)) => json!({
            "status": "ok",
            "response": {"type": "cancel", "data": {"statuses": st.iter().map(|s| match s {
                SimpleStatus::Success => Value::from("success"),
                SimpleStatus::Error(e) => json!({"error": e}),
            }).collect::<Vec<_>>()}},
        }),
        Ok(ActionResponse::Default) => json!({"status": "ok", "response": {"type": "default"}}),
        Err(e) => err(e),
    }
}

pub fn err(msg: &str) -> Value {
    json!({"status": "err", "response": msg})
}

// ---------------------------------------------------- fast /exchange path ---

fn push_json_str(out: &mut String, s: &str) {
    out.push_str(&serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into()));
}

/// Same JSON as [`action_result`] (byte-identical to `action_result(r).to_string()`),
/// written directly into a string (hot path: ~20x faster than building a Value).
pub fn action_result_string(r: &Result<ActionResponse, String>) -> String {
    use std::fmt::Write;
    let mut o = String::with_capacity(64);
    match r {
        Ok(ActionResponse::Order(st)) => {
            o.reserve(st.len() * 24);
            o.push_str(r#"{"status":"ok","response":{"type":"order","data":{"statuses":["#);
            for (i, s) in st.iter().enumerate() {
                if i > 0 {
                    o.push(',');
                }
                match s {
                    OrderStatusResp::Resting { oid, cloid } => {
                        let _ = write!(o, r#"{{"resting":{{"oid":{oid}"#);
                        if let Some(c) = cloid {
                            let _ = write!(o, r#","cloid":"{}""#, fmt::cloid(c));
                        }
                        o.push_str("}}");
                    }
                    OrderStatusResp::Filled { total_sz, avg_px, oid, cloid } => {
                        let _ = write!(
                            o,
                            r#"{{"filled":{{"totalSz":"{}","avgPx":"{}","oid":{oid}"#,
                            fmt::sz_abs(*total_sz),
                            fmt::px(*avg_px)
                        );
                        if let Some(c) = cloid {
                            let _ = write!(o, r#","cloid":"{}""#, fmt::cloid(c));
                        }
                        o.push_str("}}");
                    }
                    OrderStatusResp::WaitingForTrigger { .. } => o.push_str(r#""waitingForTrigger""#),
                    OrderStatusResp::WaitingForFill { .. } => o.push_str(r#""waitingForFill""#),
                    OrderStatusResp::Error(e) => {
                        o.push_str(r#"{"error":"#);
                        push_json_str(&mut o, e);
                        o.push('}');
                    }
                }
            }
            o.push_str("]}}}");
        }
        Ok(ActionResponse::Cancel(st)) => {
            o.push_str(r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["#);
            for (i, s) in st.iter().enumerate() {
                if i > 0 {
                    o.push(',');
                }
                match s {
                    SimpleStatus::Success => o.push_str(r#""success""#),
                    SimpleStatus::Error(e) => {
                        o.push_str(r#"{"error":"#);
                        push_json_str(&mut o, e);
                        o.push('}');
                    }
                }
            }
            o.push_str("]}}}");
        }
        Ok(ActionResponse::Default) => o.push_str(r#"{"status":"ok","response":{"type":"default"}}"#),
        Err(e) => {
            o.push_str(r#"{"status":"err","response":"#);
            push_json_str(&mut o, e);
            o.push('}');
        }
    }
    o
}

pub fn err_string(msg: &str) -> String {
    action_result_string(&Err(msg.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_render_is_identical() {
        let c = Some([0xabu8; 16]);
        let cases: Vec<Result<ActionResponse, String>> = vec![
            Ok(ActionResponse::Order(vec![
                OrderStatusResp::Resting { oid: 1, cloid: None },
                OrderStatusResp::Resting { oid: u64::MAX, cloid: c },
                OrderStatusResp::Filled { total_sz: Sz::parse("0.02").unwrap(), avg_px: Px::parse("1891.4").unwrap(), oid: 2, cloid: None },
                OrderStatusResp::Filled { total_sz: Sz::parse("3").unwrap(), avg_px: Px::parse("0.00001234").unwrap(), oid: 3, cloid: c },
                OrderStatusResp::WaitingForTrigger { oid: 4 },
                OrderStatusResp::WaitingForFill { oid: 5 },
                OrderStatusResp::Error("Order has \"invalid\" price.\n\\".into()),
            ])),
            Ok(ActionResponse::Order(vec![])),
            Ok(ActionResponse::Cancel(vec![SimpleStatus::Success, SimpleStatus::Error("x\u{1}y".into())])),
            Ok(ActionResponse::Default),
            Err("Insufficient margin \"quoted\"".into()),
        ];
        for r in cases {
            assert_eq!(action_result_string(&r), action_result(&r).to_string());
        }
    }

    #[test]
    fn dir_strings() {
        let s = |x: &str| Sz::parse(x).unwrap();
        assert_eq!(dir(Side::Bid, s("0"), s("1")), "Open Long");
        assert_eq!(dir(Side::Bid, s("2"), s("1")), "Open Long");
        assert_eq!(dir(Side::Bid, s("-2"), s("1")), "Close Short");
        assert_eq!(dir(Side::Bid, s("-1"), s("1")), "Close Short");
        assert_eq!(dir(Side::Bid, s("-1"), s("2")), "Short > Long");
        assert_eq!(dir(Side::Ask, s("0"), s("1")), "Open Short");
        assert_eq!(dir(Side::Ask, s("-1"), s("1")), "Open Short");
        assert_eq!(dir(Side::Ask, s("1"), s("1")), "Close Long");
        assert_eq!(dir(Side::Ask, s("1"), s("3")), "Long > Short");
    }
}
