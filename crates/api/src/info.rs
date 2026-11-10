//! `POST /info` request types.

use crate::candle::Interval;
use crate::coins::CoinRef;
use crate::fmt;
use crate::render;
use crate::shared::Shared;
use interfaces::*;
use serde_json::{json, Value};

/// Error text returned with HTTP 422 (HL: "Failed to deserialize the JSON body into the target type").
pub type InfoError = String;

const DESER: &str = "Failed to deserialize the JSON body into the target type";

fn field<'a>(v: &'a Value, k: &str) -> Result<&'a Value, InfoError> {
    v.get(k).filter(|x| !x.is_null()).ok_or_else(|| format!("{DESER}: missing field `{k}`"))
}

fn str_field<'a>(v: &'a Value, k: &str) -> Result<&'a str, InfoError> {
    field(v, k)?.as_str().ok_or_else(|| format!("{DESER}: `{k}` must be a string"))
}

fn user_field(v: &Value) -> Result<Address, InfoError> {
    let s = str_field(v, "user")?;
    fmt::parse_addr(s).ok_or_else(|| format!("{DESER}: invalid user address"))
}

fn opt_u64(v: &Value, k: &str) -> Result<Option<u64>, InfoError> {
    match v.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(x) => x
            .as_u64()
            .or_else(|| x.as_i64().map(|i| i.max(0) as u64))
            .map(Some)
            .ok_or_else(|| format!("{DESER}: `{k}` must be an integer")),
    }
}

fn dex_is_main(v: &Value) -> bool {
    match v.get("dex") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        _ => false,
    }
}

pub fn handle(s: &Shared, req: &Value) -> Result<Value, InfoError> {
    let ty = req.get("type").and_then(|t| t.as_str()).ok_or_else(|| format!("{DESER}: missing field `type`"))?;
    let coins = &s.coins;
    let now = s.now();
    Ok(match ty {
        "meta" => {
            if dex_is_main(req) {
                render::meta(coins)
            } else {
                json!({"universe": [], "marginTables": [], "collateralToken": 0})
            }
        }
        "metaAndAssetCtxs" => json!([render::meta(coins), asset_ctxs(s, now)]),
        "spotMeta" => render::spot_meta(coins),
        "spotMetaAndAssetCtxs" => json!([render::spot_meta(coins), crate::spot::ctxs(s, now)]),
        // Prediction markets are not deployed (TS SymbolConverter calls this).
        "outcomeMeta" => json!({"outcomes": [], "questions": []}),
        "allMids" => {
            if dex_is_main(req) {
                all_mids(s)
            } else {
                json!({})
            }
        }
        "l2Book" => {
            let coin = str_field(req, "coin")?;
            let n_sig = opt_u64(req, "nSigFigs")?.map(|x| x as u32);
            if let Some(n) = n_sig {
                if !(2..=5).contains(&n) {
                    return Err(format!("{DESER}: nSigFigs must be 2..5"));
                }
            }
            let mantissa = opt_u64(req, "mantissa")?.map(|x| x as u32);
            l2_book(s, coin, n_sig, mantissa).unwrap_or(Value::Null)
        }
        "clearinghouseState" => {
            let user = user_field(req)?;
            if dex_is_main(req) {
                let a = s.with_state(|q| q.account(&user));
                render::clearinghouse(coins, &a, now)
            } else {
                render::clearinghouse(coins, &AccountView::default(), now)
            }
        }
        "spotClearinghouseState" => {
            let user = user_field(req)?;
            crate::spot::clearinghouse(s, &user)
        }
        "openOrders" => {
            let user = user_field(req)?;
            if !dex_is_main(req) {
                return Ok(json!([]));
            }
            let orders = sorted_open_orders(s, &user);
            Value::Array(orders.iter().map(|o| render::open_order(coins, o)).collect())
        }
        "frontendOpenOrders" => {
            let user = user_field(req)?;
            if !dex_is_main(req) {
                return Ok(json!([]));
            }
            let orders = sorted_open_orders(s, &user);
            Value::Array(orders.iter().map(|o| render::frontend_order(coins, o)).collect())
        }
        "orderStatus" => {
            let user = user_field(req)?;
            let oid = field(req, "oid")?;
            order_status(s, &user, oid)?
        }
        "userFills" => {
            let user = user_field(req)?;
            let mut fills = s.index.read().user_fills(&user, 2000);
            if req.get("aggregateByTime").and_then(|x| x.as_bool()).unwrap_or(false) {
                fills.reverse();
                fills = render::aggregate_fills(fills);
                fills.reverse();
            }
            Value::Array(fills.iter().map(|f| render::fill(coins, f)).collect())
        }
        "userFillsByTime" => {
            let user = user_field(req)?;
            let start = opt_u64(req, "startTime")?.unwrap_or(0);
            let end = opt_u64(req, "endTime")?.unwrap_or(u64::MAX);
            let mut fills = s.index.read().user_fills_by_time(&user, start, end, 2000);
            if req.get("aggregateByTime").and_then(|x| x.as_bool()).unwrap_or(false) {
                fills = render::aggregate_fills(fills);
            }
            Value::Array(fills.iter().map(|f| render::fill(coins, f)).collect())
        }
        "userFunding" => {
            let user = user_field(req)?;
            let start = opt_u64(req, "startTime")?.unwrap_or(0);
            let end = opt_u64(req, "endTime")?.unwrap_or(u64::MAX);
            let v = s.index.read().user_funding(&user, start, end);
            Value::Array(v.iter().take(500).map(|f| render::user_funding(coins, f)).collect())
        }
        "userNonFundingLedgerUpdates" => {
            let user = user_field(req)?;
            let start = opt_u64(req, "startTime")?.unwrap_or(0);
            let end = opt_u64(req, "endTime")?.unwrap_or(u64::MAX);
            let v = s.index.read().user_ledger(&user, start, end);
            Value::Array(v.iter().take(500).map(|l| render::ledger(coins, l)).collect())
        }
        "fundingHistory" => {
            let coin = str_field(req, "coin")?;
            let start = opt_u64(req, "startTime")?.unwrap_or(0);
            let end = opt_u64(req, "endTime")?.unwrap_or(u64::MAX);
            match coins.perp(coin) {
                Some(a) => {
                    let pts = s.index.read().funding_history(a, start, end);
                    Value::Array(
                        pts.iter()
                            .take(500)
                            .map(|p| {
                                json!({"coin": coin, "fundingRate": fmt::rate(p.rate), "premium": fmt::rate(p.premium), "time": p.time})
                            })
                            .collect(),
                    )
                }
                None => json!([]),
            }
        }
        "candleSnapshot" => {
            let r = field(req, "req")?;
            let coin = str_field(r, "coin")?;
            let iv = Interval::parse(str_field(r, "interval")?).ok_or_else(|| format!("{DESER}: bad interval"))?;
            let start = opt_u64(r, "startTime")?.unwrap_or(0);
            let end = opt_u64(r, "endTime")?.unwrap_or(u64::MAX);
            match coins.asset(coin) {
                Some(a) => {
                    let cs = s.index.read().candles(a, iv, start, end);
                    let skip = cs.len().saturating_sub(5000);
                    Value::Array(cs[skip..].iter().map(|c| render::candle(coin, iv, c)).collect())
                }
                None => json!([]),
            }
        }
        "historicalOrders" => {
            let user = user_field(req)?;
            let v = s.index.read().historical_orders(&user, 2000);
            Value::Array(v.iter().map(|r| render::order_status_entry(coins, r)).collect())
        }
        "userFees" => {
            let user = user_field(req)?;
            user_fees(s, &user)
        }
        "userRateLimit" => {
            let user = user_field(req)?;
            let idx = s.index.read();
            let vlm = idx.cum_vlm(&user);
            let used = idx.n_requests(&user);
            json!({
                "cumVlm": fmt::dec(vlm, USD_DECIMALS),
                "nRequestsUsed": used,
                "nRequestsCap": 10_000u64 + (vlm / USD_SCALE).max(0) as u64,
                "nRequestsSurplus": 0,
            })
        }
        "perpDexs" => json!([null]),
        "referral" => {
            user_field(req)?;
            json!({
                "referredBy": null,
                "cumVlm": "0.0",
                "unclaimedRewards": "0.0",
                "claimedRewards": "0.0",
                "builderRewards": "0.0",
                "referrerState": {"stage": "needToCreateCode"},
                "rewardHistory": [],
                "tokenToState": [],
            })
        }
        "maxBuilderFee" => {
            user_field(req)?;
            json!(0)
        }
        "recentTrades" => {
            let coin = str_field(req, "coin")?;
            match coins.asset(coin) {
                Some(a) => {
                    let v = s.index.read().recent_trades(a, 100);
                    Value::Array(v.iter().map(|t| render::trade(coins, t)).collect())
                }
                None => json!([]),
            }
        }
        "activeAssetData" => {
            let user = user_field(req)?;
            let coin = str_field(req, "coin")?;
            active_asset_data(s, &user, coin).ok_or_else(|| format!("{DESER}: unknown coin"))?
        }
        "predictedFundings" => predicted_fundings(s, now),
        "exchangeStatus" => json!({"time": now, "specialStatuses": null}),
        "webData2" => {
            let user = user_field(req)?;
            web_data2(s, &user, now)
        }
        "extraAgents" | "userTwapSliceFills" | "twapHistory" | "userVaultEquities" | "delegations" | "leadingVaults" => {
            json!([])
        }
        "subAccounts" => Value::Null,
        "userRole" => {
            let user = user_field(req)?;
            let principal = s.with_state(|q| q.principal_of(&user));
            if principal != user {
                json!({"role": "agent", "data": {"user": fmt::addr(&principal)}})
            } else {
                json!({"role": "user"})
            }
        }
        "pendingWithdrawals" => {
            let v = s.with_state(|q| q.pending_withdrawals());
            Value::Array(v.iter().map(pending_withdrawal).collect())
        }
        "nodeStatus" => serde_json::to_value(s.backend.status()).unwrap_or(Value::Null),
        other => return Err(format!("{DESER}: unknown variant `{other}`")),
    })
}

/// Shape agreed with crates/relayer (`relayer::api::to_wire`): amount is the
/// decimal USD received by the destination (net of fee), `created_at` in ms,
/// signatures flattened to `{validator, r, s, v}`.
pub fn pending_withdrawal(w: &PendingWithdrawal) -> Value {
    json!({
        "id": w.id,
        "user": fmt::addr(&w.user),
        "destination": fmt::addr(&w.destination),
        "amount": w.amount.to_wire(),
        "created_at": w.created_at,
        "signatures": w.signatures.iter().map(|(a, sig)| json!({
            "validator": fmt::addr(a),
            "r": format!("0x{}", hex::encode(sig.r)),
            "s": format!("0x{}", hex::encode(sig.s)),
            "v": sig.v,
        })).collect::<Vec<_>>(),
    })
}

fn sorted_open_orders(s: &Shared, user: &Address) -> Vec<RestingOrderView> {
    let mut v = s.with_state(|q| q.open_orders(user));
    // HL: most recent first
    v.sort_by(|a, b| (b.timestamp, b.oid).cmp(&(a.timestamp, a.oid)));
    v
}

pub fn asset_ctxs(s: &Shared, now: u64) -> Value {
    let markets = &s.coins.markets;
    let ctxs: Vec<(MarketCtx, Option<(Px, Px)>)> = s.with_state(|q| {
        markets
            .iter()
            .map(|m| {
                let (b, a) = q.l2_book(m.asset, 1);
                let impact = match (b.first(), a.first()) {
                    (Some(b), Some(a)) => Some((b.0, a.0)),
                    _ => None,
                };
                (q.market_ctx(m.asset), impact)
            })
            .collect()
    });
    let idx = s.index.read();
    Value::Array(ctxs.iter().map(|(c, i)| render::asset_ctx(c, *i, idx.day_base_vlm(c.asset, now))).collect())
}

pub fn asset_ctx_one(s: &Shared, asset: AssetId, now: u64) -> Value {
    let (c, impact) = s.with_state(|q| {
        let (b, a) = q.l2_book(asset, 1);
        let impact = match (b.first(), a.first()) {
            (Some(b), Some(a)) => Some((b.0, a.0)),
            _ => None,
        };
        (q.market_ctx(asset), impact)
    });
    render::asset_ctx(&c, impact, s.index.read().day_base_vlm(asset, now))
}

pub fn all_mids(s: &Shared) -> Value {
    let markets = &s.coins.markets;
    let mids: Vec<(String, Px)> = s.with_state(|q| {
        markets
            .iter()
            .map(|m| {
                let px = q.mid(m.asset).unwrap_or_else(|| {
                    let c = q.market_ctx(m.asset);
                    if c.mark_px.0 > 0 {
                        c.mark_px
                    } else if c.oracle_px.0 > 0 {
                        c.oracle_px
                    } else {
                        m.initial_px
                    }
                });
                (m.name.clone(), px)
            })
            .collect()
    });
    let mut o = serde_json::Map::new();
    for (n, p) in mids.into_iter().chain(crate::spot::mids(s)) {
        o.insert(n, fmt::px(p).into());
    }
    Value::Object(o)
}

/// l2Book JSON for `coin`; None if the coin is unknown.
pub fn l2_book(s: &Shared, coin: &str, n_sig: Option<u32>, mantissa: Option<u32>) -> Option<Value> {
    let time = s.now();
    match s.coins.resolve(coin)? {
        CoinRef::Spot(p) if s.coins.tradable_spot_asset(p).is_none() => {
            Some(json!({"coin": coin, "time": time, "levels": [[], []]}))
        }
        r => {
            let a = match r {
                CoinRef::Perp(a) => a,
                CoinRef::Spot(p) => s.coins.tradable_spot_asset(p).expect("tradable"),
            };
            let depth = if n_sig.is_some() { 2000 } else { 20 };
            let (bids, asks) = s.with_state(|q| q.l2_book(a, depth));
            let bids = render::aggregate_levels(&bids, n_sig, mantissa, false, 20);
            let asks = render::aggregate_levels(&asks, n_sig, mantissa, true, 20);
            Some(json!({"coin": coin, "time": time, "levels": [render::l2_levels(&bids), render::l2_levels(&asks)]}))
        }
    }
}

/// Best bid / offer for the WS `bbo` channel.
pub fn bbo(s: &Shared, coin: &str) -> Option<Value> {
    let time = s.now();
    let lvl = |l: Option<&(Px, Sz, u32)>| match l {
        Some((p, z, n)) => json!({"px": fmt::px(*p), "sz": fmt::sz_abs(*z), "n": n}),
        None => Value::Null,
    };
    match s.coins.resolve(coin)? {
        CoinRef::Spot(p) if s.coins.tradable_spot_asset(p).is_none() => {
            Some(json!({"coin": coin, "time": time, "bbo": [null, null]}))
        }
        r => {
            let a = match r {
                CoinRef::Perp(a) => a,
                CoinRef::Spot(p) => s.coins.tradable_spot_asset(p).expect("tradable"),
            };
            let (b, k) = s.with_state(|q| q.l2_book(a, 1));
            Some(json!({"coin": coin, "time": time, "bbo": [lvl(b.first()), lvl(k.first())]}))
        }
    }
}

fn order_status(s: &Shared, user: &Address, oid: &Value) -> Result<Value, InfoError> {
    let coins = &s.coins;
    enum Key {
        Oid(Oid),
        Cloid(Cloid),
    }
    let key = if let Some(n) = oid.as_u64() {
        Key::Oid(n)
    } else if let Some(st) = oid.as_str() {
        match fmt::parse_cloid(st) {
            Some(c) => Key::Cloid(c),
            None => match st.parse::<u64>() {
                Ok(n) => Key::Oid(n),
                Err(_) => return Err(format!("{DESER}: invalid oid")),
            },
        }
    } else {
        return Err(format!("{DESER}: invalid oid"));
    };
    let live = s.with_state(|q| match &key {
        Key::Oid(o) => q.order_by_oid(user, *o),
        Key::Cloid(c) => q.order_by_cloid(user, c),
    });
    let hist = {
        let idx = s.index.read();
        match &key {
            Key::Oid(o) => idx.order(user, *o),
            Key::Cloid(c) => idx.order_by_cloid(user, c),
        }
    };
    let entry = match (live, hist) {
        (Some(o), h) => {
            let (status, ts) = match h {
                Some(h) if h.order.oid == o.oid && h.status == OrderLifecycle::Triggered => (h.status, h.status_ts),
                Some(h) if h.order.oid == o.oid && h.status == OrderLifecycle::Open => (OrderLifecycle::Open, h.status_ts),
                _ => (OrderLifecycle::Open, o.timestamp),
            };
            json!({"order": render::frontend_order(coins, &o), "status": render::status_str(status), "statusTimestamp": ts})
        }
        (None, Some(h)) => render::order_status_entry(coins, &h),
        (None, None) => return Ok(json!({"status": "unknownOid"})),
    };
    Ok(json!({"status": "order", "order": entry}))
}

fn user_fees(s: &Shared, user: &Address) -> Value {
    let p = &s.genesis.params;
    let taker = fmt::rate(p.taker_fee);
    let maker = fmt::rate(p.maker_fee);
    let days = s.index.read().daily_volumes(user, 15);
    let daily: Vec<Value> = days
        .iter()
        .map(|(d, c, a, e)| {
            json!({"date": d, "userCross": fmt::dec(*c, USD_DECIMALS), "userAdd": fmt::dec(*a, USD_DECIMALS), "exchange": fmt::dec(*e, USD_DECIMALS)})
        })
        .collect();
    json!({
        "dailyUserVlm": daily,
        "feeSchedule": {
            "cross": taker,
            "add": maker,
            "spotCross": taker,
            "spotAdd": maker,
            "tiers": {"vip": [], "mm": []},
            "referralDiscount": "0.0",
            "stakingDiscountTiers": [],
        },
        "userCrossRate": taker,
        "userAddRate": maker,
        "userSpotCrossRate": taker,
        "userSpotAddRate": maker,
        "activeReferralDiscount": "0.0",
        "trial": null,
        "feeTrialEscrow": "0.0",
        "nextTrialAvailableTimestamp": null,
        "stakingLink": null,
        "activeStakingDiscount": {"bpsOfMaxSupply": "0.0", "discount": "0.0"},
    })
}

/// Default leverage for an asset without a position (HL: 20x cross, capped).
pub fn default_leverage(m: &MarketSpec) -> Leverage {
    let v = m.max_leverage.min(20).max(1);
    if m.only_isolated {
        Leverage::Isolated(v)
    } else {
        Leverage::Cross(v)
    }
}

pub fn active_asset_data(s: &Shared, user: &Address, coin: &str) -> Option<Value> {
    let asset = s.coins.perp(coin)?;
    let m = s.coins.market(asset)?.clone();
    let (acct, ctx, setting) = s.with_state(|q| (q.account(user), q.market_ctx(asset), q.leverage_of(user, asset)));
    let pos = acct.positions.iter().find(|p| p.asset == asset && !p.szi.is_zero());
    let (lev, raw) = match pos {
        Some(p) => (p.leverage, p.margin_used),
        None => (setting.unwrap_or_else(|| default_leverage(&m)), Usd::ZERO),
    };
    let avail = acct.withdrawable.raw().max(0) * lev.value() as i128;
    let mark = if ctx.mark_px.0 > 0 { ctx.mark_px.raw() } else { m.initial_px.raw().max(1) };
    let sz_raw = mul_div(avail, 10_000_000_000, mark, Rounding::Floor);
    let unit = 10i128.pow(SZ_DECIMALS.saturating_sub(m.sz_decimals));
    let sz_raw = sz_raw / unit * unit;
    let szs = fmt::dec(sz_raw, SZ_DECIMALS);
    let av = fmt::dec(avail, USD_DECIMALS);
    Some(json!({
        "user": fmt::addr(user),
        "coin": coin,
        "leverage": render::leverage(&lev, raw),
        "maxTradeSzs": [szs, szs],
        "availableToTrade": [av, av],
        "markPx": fmt::px(Px::from_raw(mark)),
    }))
}

fn predicted_fundings(s: &Shared, now: u64) -> Value {
    let interval = s.genesis.params.funding_interval_ms.max(1);
    let next = (now / interval + 1) * interval;
    let hours = (interval / 3_600_000).max(1);
    let markets = &s.coins.markets;
    let rates: Vec<(String, Rate)> = s.with_state(|q| markets.iter().map(|m| (m.name.clone(), q.market_ctx(m.asset).funding)).collect());
    Value::Array(
        rates
            .into_iter()
            .map(|(n, r)| json!([n, [["HlPerp", {"fundingRate": fmt::rate(r), "nextFundingTime": next, "fundingIntervalHours": hours}]]]))
            .collect(),
    )
}

pub fn web_data2(s: &Shared, user: &Address, now: u64) -> Value {
    let coins = &s.coins;
    let acct = s.with_state(|q| q.account(user));
    let orders = sorted_open_orders(s, user);
    let spot_ctxs = crate::spot::ctxs(s, now);
    json!({
        "clearinghouseState": render::clearinghouse(coins, &acct, now),
        "leadingVaults": [],
        "totalVaultEquity": "0.0",
        "openOrders": orders.iter().map(|o| render::frontend_order(coins, o)).collect::<Vec<_>>(),
        "agentAddress": null,
        "agentValidUntil": null,
        "cumLedger": "0.0",
        "meta": render::meta(coins),
        "assetCtxs": asset_ctxs(s, now),
        "serverTime": now,
        "isVault": false,
        "user": fmt::addr(user),
        "twapStates": [],
        "spotState": crate::spot::clearinghouse_from(coins, &s.genesis.params, &acct, &orders),
        "spotAssetCtxs": spot_ctxs,
    })
}
