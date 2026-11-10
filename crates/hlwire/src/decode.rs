//! HL wire msgpack -> typed [`interfaces::Action`], plus typed wire helpers
//! (cloids, addresses, exact decimal strings).

use crate::msgpack::{ArrIter, Mp, Reader};
use interfaces::*;

// ------------------------------------------------------------ wire helpers

/// Parse a cloid: `0x` + 32 hex digits (any case).
pub fn parse_cloid(s: &str) -> Result<Cloid, String> {
    let b = s.as_bytes();
    if b.len() != 34 || b[0] != b'0' || (b[1] != b'x' && b[1] != b'X') {
        return Err(format!("cloid must be 0x + 16 bytes hex: {s}"));
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        match (nibble(b[2 + 2 * i]), nibble(b[3 + 2 * i])) {
            (Some(h), Some(l)) => *o = (h << 4) | l,
            _ => return Err(format!("bad cloid hex: {s}")),
        }
    }
    Ok(out)
}

#[inline]
fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Lowercase `0x`-prefixed hex form of a cloid (what both SDKs send).
pub fn cloid_to_hex(c: &Cloid) -> String {
    format!("0x{}", hex::encode(c))
}

/// Cloid from an integer, like Python `Cloid.from_int`.
pub fn cloid_from_u128(v: u128) -> Cloid {
    v.to_be_bytes()
}

/// Parse an address: optional `0x`, 40 hex digits, any case (checksum not
/// enforced, matching HL which lowercases addresses).
pub fn parse_address(s: &str) -> Result<Address, String> {
    let h = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if h.len() != 40 {
        return Err(format!("invalid address: {s}"));
    }
    let mut out = [0u8; 20];
    hex::decode_to_slice(h, &mut out).map_err(|_| format!("invalid address: {s}"))?;
    Ok(Address(out))
}

/// Exact decimal wire string -> integer with `decimals` implied places.
///
/// Accepts plain decimals ("123", "0.25", ".5", "1.") and, because Python's
/// SDK formats some amounts with `str(float)`, scientific notation
/// ("1e-05", "2.5E3"). Never rounds: more fractional precision than
/// `decimals` (after trimming trailing zeros) is an error. No whitespace.
pub fn parse_wire_decimal(s: &str, decimals: u32) -> Result<i128, String> {
    if s.bytes().any(|c| c == b'e' || c == b'E') {
        return parse_scientific(s, decimals);
    }
    parse_plain(s.as_bytes(), decimals).map_err(|e| format!("{e}: {s:?}"))
}

/// Fast exact parser for plain decimals (no exponent). Values must fit
/// 64 bits once scaled (true for every Px/Sz/Usd).
fn parse_plain(b: &[u8], decimals: u32) -> Result<i128, &'static str> {
    let (neg, mut i) = match b.first() {
        Some(b'-') => (true, 1),
        Some(b'+') => (false, 1),
        Some(_) => (false, 0),
        None => return Err("invalid decimal"),
    };
    let mut acc: u64 = 0;
    let mut ndigits = 0usize;
    while i < b.len() && b[i].is_ascii_digit() {
        acc = acc
            .checked_mul(10)
            .and_then(|x| x.checked_add((b[i] - b'0') as u64))
            .ok_or("decimal overflow")?;
        i += 1;
        ndigits += 1;
    }
    let mut frac = 0u32;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            let d = (b[i] - b'0') as u64;
            if frac < decimals {
                acc = acc
                    .checked_mul(10)
                    .and_then(|x| x.checked_add(d))
                    .ok_or("decimal overflow")?;
                frac += 1;
            } else if d != 0 {
                return Err("too much precision");
            }
            i += 1;
            ndigits += 1;
        }
    }
    if i != b.len() || ndigits == 0 {
        return Err("invalid decimal");
    }
    let scale = 10u64
        .checked_pow(decimals - frac)
        .ok_or("decimal overflow")?;
    let v = acc.checked_mul(scale).ok_or("decimal overflow")? as i128;
    Ok(if neg { -v } else { v })
}

fn parse_scientific(s: &str, decimals: u32) -> Result<i128, String> {
    let i = s.find(['e', 'E']).expect("caller checked");
    let ev: i32 = s[i + 1..]
        .parse()
        .map_err(|_| format!("invalid decimal: {s:?}"))?;
    if !(-60..=60).contains(&ev) {
        return Err(format!("decimal exponent out of range: {s:?}"));
    }
    let plain = shift_decimal(&s[..i], ev).ok_or_else(|| format!("invalid decimal: {s:?}"))?;
    parse_plain(plain.as_bytes(), decimals).map_err(|e| format!("{e}: {s:?}"))
}

/// Rewrite a plain decimal mantissa multiplied by 10^exp as a plain decimal.
fn shift_decimal(m: &str, exp: i32) -> Option<String> {
    let (sign, body) = match m.as_bytes().first()? {
        b'-' => ("-", &m[1..]),
        b'+' => ("", &m[1..]),
        _ => ("", m),
    };
    let (ip, fp) = match body.find('.') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None => (body, ""),
    };
    if (ip.is_empty() && fp.is_empty()) || !ip.bytes().chain(fp.bytes()).all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{ip}{fp}");
    let point = ip.len() as i64 + exp as i64; // position of the decimal point in `digits`
    let s = if point <= 0 {
        format!("0.{}{}", "0".repeat((-point) as usize), digits)
    } else if point as usize >= digits.len() {
        format!("{}{}", digits, "0".repeat(point as usize - digits.len()))
    } else {
        format!(
            "{}.{}",
            &digits[..point as usize],
            &digits[point as usize..]
        )
    };
    Some(format!("{sign}{s}"))
}

/// Wire price string -> [`Px`] (exact, > 0 not enforced here).
pub fn parse_px(s: &str) -> Result<Px, String> {
    let v = parse_wire_decimal(s, PX_DECIMALS)?;
    u64::try_from(v)
        .map(Px)
        .map_err(|_| format!("invalid price: {s}"))
}

/// Wire size string -> [`Sz`] (exact, non-negative; "0" allowed for
/// positionTpsl orders).
pub fn parse_sz(s: &str) -> Result<Sz, String> {
    let v = parse_wire_decimal(s, SZ_DECIMALS)?;
    if v < 0 {
        return Err(format!("invalid size: {s}"));
    }
    i64::try_from(v)
        .map(Sz)
        .map_err(|_| format!("invalid size: {s}"))
}

/// Wire USD amount string -> [`Usd`] (exact, may be negative).
pub fn parse_usd(s: &str) -> Result<Usd, String> {
    let v = parse_wire_decimal(s, USD_DECIMALS)?;
    i64::try_from(v)
        .map(Usd)
        .map_err(|_| format!("invalid amount: {s}"))
}

/// Wire USD amount string -> non-negative [`Usd`].
pub fn parse_usd_amount(s: &str) -> Result<Usd, String> {
    let u = parse_usd(s)?;
    if u.0 < 0 {
        return Err(format!("invalid amount: {s}"));
    }
    Ok(u)
}

// ------------------------------------------------------------ field access

type R<T> = Result<T, String>;

fn field<'a>(m: &Mp<'a>, k: &str) -> R<Mp<'a>> {
    match m.get(k) {
        Some(v) => Ok(v),
        None => Err(format!("missing field `{k}`")),
    }
}
/// Present and not nil.
fn opt_field<'a>(m: &Mp<'a>, k: &str) -> Option<Mp<'a>> {
    match m.get(k) {
        None | Some(Mp::Nil) => None,
        Some(v) => Some(v),
    }
}
fn as_str<'a>(v: &Mp<'a>, k: &str) -> R<&'a str> {
    match v {
        Mp::Str(s) => Ok(s),
        o => Err(format!("field `{k}`: expected string, got {}", o.kind())),
    }
}
fn as_bool(v: &Mp<'_>, k: &str) -> R<bool> {
    match v {
        Mp::Bool(b) => Ok(*b),
        o => Err(format!("field `{k}`: expected bool, got {}", o.kind())),
    }
}
fn as_u64(v: &Mp<'_>, k: &str) -> R<u64> {
    match v {
        Mp::UInt(u) => Ok(*u),
        o => Err(format!(
            "field `{k}`: expected unsigned integer, got {}",
            o.kind()
        )),
    }
}
fn as_u32(v: &Mp<'_>, k: &str) -> R<u32> {
    let u = as_u64(v, k)?;
    u32::try_from(u).map_err(|_| format!("field `{k}`: value {u} out of range"))
}
fn as_i64(v: &Mp<'_>, k: &str) -> R<i64> {
    match v {
        Mp::UInt(u) => i64::try_from(*u).map_err(|_| format!("field `{k}`: value out of range")),
        Mp::NegInt(i) => Ok(*i),
        o => Err(format!("field `{k}`: expected integer, got {}", o.kind())),
    }
}
fn as_arr<'a>(v: &Mp<'a>, k: &str) -> R<ArrIter<'a>> {
    match v {
        Mp::Arr(a) => Ok(a.items()),
        o => Err(format!("field `{k}`: expected array, got {}", o.kind())),
    }
}
fn str_f<'a>(m: &Mp<'a>, k: &str) -> R<&'a str> {
    as_str(&field(m, k)?, k)
}
fn bool_f(m: &Mp<'_>, k: &str) -> R<bool> {
    as_bool(&field(m, k)?, k)
}
fn u64_f(m: &Mp<'_>, k: &str) -> R<u64> {
    as_u64(&field(m, k)?, k)
}
fn u32_f(m: &Mp<'_>, k: &str) -> R<u32> {
    as_u32(&field(m, k)?, k)
}
fn addr_f(m: &Mp<'_>, k: &str) -> R<Address> {
    parse_address(str_f(m, k)?)
}
fn cloid_v(v: &Mp<'_>, k: &str) -> R<Cloid> {
    parse_cloid(as_str(v, k)?)
}
fn require_map(m: &Mp<'_>, what: &str) -> R<()> {
    match m {
        Mp::Map(_) => Ok(()),
        o => Err(format!("{what}: expected map, got {}", o.kind())),
    }
}

// ------------------------------------------------------------ actions

/// Parse an HL OrderWire map `{a,b,p,s,r,t,c?}` (unknown keys ignored,
/// first occurrence of a key wins).
fn order_wire(m: &Mp<'_>) -> R<OrderRequest> {
    let Mp::Map(seq) = m else {
        return Err(format!("order: expected map, got {}", m.kind()));
    };
    let mut r = seq.reader();
    order_wire_body(&mut r, seq.n)
}

/// Streaming OrderWire parse from a validated reader positioned at a map
/// body with `n` entries (single pass, no intermediate views).
fn order_wire_body(r: &mut Reader<'_>, n: usize) -> R<OrderRequest> {
    let (mut a, mut b, mut p, mut sz, mut ro, mut kind, mut c) =
        (None, None, None, None, None, None, None);
    for _ in 0..n {
        let k = r.read_str_bytes()?;
        match k {
            b"a" if a.is_none() => a = Some(as_u32(&r.read_value()?, "a")?),
            b"b" if b.is_none() => b = Some(as_bool(&r.read_value()?, "b")?),
            b"p" if p.is_none() => {
                p = Some(parse_px(
                    r.read_str().map_err(|e| format!("field `p`: {e}"))?,
                )?)
            }
            b"s" if sz.is_none() => {
                sz = Some(parse_sz(
                    r.read_str().map_err(|e| format!("field `s`: {e}"))?,
                )?)
            }
            b"r" if ro.is_none() => ro = Some(as_bool(&r.read_value()?, "r")?),
            b"t" if kind.is_none() => kind = Some(order_type(&r.read_value()?)?),
            b"c" if c.is_none() => {
                c = Some(if r.peek() == Some(0xc0) {
                    r.skip_value()?;
                    None
                } else {
                    Some(parse_cloid(
                        r.read_str().map_err(|e| format!("field `c`: {e}"))?,
                    )?)
                })
            }
            _ => r.skip_value()?,
        }
    }
    let miss = |k: &str| format!("missing field `{k}`");
    Ok(OrderRequest {
        asset: a.ok_or_else(|| miss("a"))?,
        is_buy: b.ok_or_else(|| miss("b"))?,
        limit_px: p.ok_or_else(|| miss("p"))?,
        sz: sz.ok_or_else(|| miss("s"))?,
        reduce_only: ro.ok_or_else(|| miss("r"))?,
        kind: kind.ok_or_else(|| miss("t"))?,
        cloid: c.flatten(),
    })
}

fn order_type(t: &Mp<'_>) -> R<OrderKind> {
    require_map(t, "order type `t`")?;
    if let Some(l) = opt_field(t, "limit") {
        let tif = match str_f(&l, "tif")? {
            "Gtc" => Tif::Gtc,
            "Ioc" => Tif::Ioc,
            "Alo" => Tif::Alo,
            // TS SDK: "Similar to Ioc, but add a note that this is market order".
            "FrontendMarket" => Tif::Ioc,
            other => return Err(format!("invalid tif: {other}")),
        };
        Ok(OrderKind::Limit { tif })
    } else if let Some(tr) = opt_field(t, "trigger") {
        let is_market = bool_f(&tr, "isMarket")?;
        let trigger_px = parse_px(str_f(&tr, "triggerPx")?)?;
        let tpsl = match str_f(&tr, "tpsl")? {
            "tp" => TpSl::Tp,
            "sl" => TpSl::Sl,
            other => return Err(format!("invalid tpsl: {other}")),
        };
        Ok(OrderKind::Trigger {
            trigger_px,
            is_market,
            tpsl,
        })
    } else {
        Err("order type must be `limit` or `trigger`".into())
    }
}

/// All orders of an `orders` array, streaming.
fn orders_array(v: &Mp<'_>) -> R<Vec<OrderRequest>> {
    let Mp::Arr(seq) = v else {
        return Err(format!("field `orders`: expected array, got {}", v.kind()));
    };
    let mut r = seq.reader();
    let mut out = Vec::with_capacity(seq.n);
    for _ in 0..seq.n {
        let n = r
            .read_map_len()
            .map_err(|_| "order: expected map".to_string())?;
        out.push(order_wire_body(&mut r, n)?);
    }
    Ok(out)
}

fn oid_or_cloid(v: &Mp<'_>) -> R<OidOrCloid> {
    match v {
        Mp::UInt(u) => Ok(OidOrCloid::Oid(*u)),
        Mp::Str(s) => parse_cloid(s).map(OidOrCloid::Cloid),
        o => Err(format!(
            "field `oid`: expected integer or cloid string, got {}",
            o.kind()
        )),
    }
}

/// Top-level `type` of an action given its msgpack bytes.
pub fn action_type(action_bytes: &[u8]) -> Result<String, String> {
    let m = Reader::parse_all(action_bytes)?;
    require_map(&m, "action")?;
    Ok(str_f(&m, "type")?.to_string())
}

/// True for user-signed (EIP-712 `HyperliquidTransaction:*`) action types.
pub fn is_user_signed_type(t: &str) -> bool {
    crate::sign::user_signed_types(t).is_some()
}

/// Decode HL wire msgpack into a typed [`Action`].
pub fn decode_action(action_bytes: &[u8]) -> Result<Action, String> {
    let m = Reader::parse_all(action_bytes)?;
    decode_mp(&m)
}

/// `order` with a single pass over the top-level map (hot path).
fn decode_order_fast(m: &Mp<'_>) -> Result<Action, String> {
    let Mp::Map(seq) = m else { unreachable!() };
    let (mut orders, mut grouping_v) = (None, None);
    for (k, v) in seq.entries() {
        match k {
            "orders" if orders.is_none() => orders = Some(v),
            "grouping" if grouping_v.is_none() => grouping_v = Some(v),
            _ => {} // "type" (checked by caller), "builder" (ignored)
        }
    }
    let orders = orders_array(&orders.ok_or("missing field `orders`")?)?;
    let grouping = match grouping_v {
        None | Some(Mp::Nil) => Grouping::Na,
        Some(Mp::Str("na")) => Grouping::Na,
        Some(Mp::Str("normalTpsl")) => Grouping::NormalTpsl,
        Some(Mp::Str("positionTpsl")) => Grouping::PositionTpsl,
        // Priority grouping {p: n} (TS SDK): priority fees are not modeled.
        Some(g @ Mp::Map(_)) => {
            u64_f(&g, "p")?;
            Grouping::Na
        }
        Some(Mp::Str(s)) => return Err(format!("invalid grouping: {s}")),
        Some(o) => return Err(format!("invalid grouping: {}", o.kind())),
    };
    // `builder` (builder fee) is accepted and ignored.
    Ok(Action::Order { orders, grouping })
}

pub(crate) fn decode_mp(m: &Mp<'_>) -> Result<Action, String> {
    require_map(m, "action")?;
    let t = str_f(m, "type")?;
    if t == "order" {
        return decode_order_fast(m);
    }
    Ok(match t {
        "cancel" => {
            let cancels = as_arr(&field(m, "cancels")?, "cancels")?
                .map(|c| {
                    let c = &c;
                    require_map(c, "cancel")?;
                    Ok((u32_f(c, "a")?, u64_f(c, "o")?))
                })
                .collect::<R<Vec<_>>>()?;
            Action::Cancel { cancels }
        }
        "cancelByCloid" => {
            let cancels = as_arr(&field(m, "cancels")?, "cancels")?
                .map(|c| {
                    let c = &c;
                    require_map(c, "cancel")?;
                    Ok((u32_f(c, "asset")?, cloid_v(&field(c, "cloid")?, "cloid")?))
                })
                .collect::<R<Vec<_>>>()?;
            Action::CancelByCloid { cancels }
        }
        "modify" => Action::Modify {
            oid: oid_or_cloid(&field(m, "oid")?)?,
            order: order_wire(&field(m, "order")?)?,
        },
        "batchModify" => {
            let modifies = as_arr(&field(m, "modifies")?, "modifies")?
                .map(|x| {
                    let x = &x;
                    require_map(x, "modify")?;
                    Ok((
                        oid_or_cloid(&field(x, "oid")?)?,
                        order_wire(&field(x, "order")?)?,
                    ))
                })
                .collect::<R<Vec<_>>>()?;
            Action::BatchModify { modifies }
        }
        "scheduleCancel" => {
            let time = match opt_field(m, "time") {
                Some(v) => Some(as_u64(&v, "time")?),
                None => None,
            };
            Action::ScheduleCancel { time }
        }
        "updateLeverage" => Action::UpdateLeverage {
            asset: u32_f(m, "asset")?,
            is_cross: bool_f(m, "isCross")?,
            leverage: u32_f(m, "leverage")?,
        },
        "updateIsolatedMargin" => Action::UpdateIsolatedMargin {
            asset: u32_f(m, "asset")?,
            is_buy: bool_f(m, "isBuy")?,
            ntli: Usd(as_i64(&field(m, "ntli")?, "ntli")?),
        },
        "usdSend" => Action::UsdSend {
            destination: addr_f(m, "destination")?,
            amount: parse_usd_amount(str_f(m, "amount")?)?,
            time: u64_f(m, "time")?,
        },
        "spotSend" => Action::SpotSend {
            destination: addr_f(m, "destination")?,
            token: str_f(m, "token")?.to_string(),
            amount: parse_usd_amount(str_f(m, "amount")?)?,
            time: u64_f(m, "time")?,
        },
        "usdClassTransfer" => {
            let a = str_f(m, "amount")?;
            if a.contains("subaccount:") {
                return Err("usdClassTransfer to/from a subaccount is not supported".into());
            }
            Action::UsdClassTransfer {
                amount: parse_usd_amount(a)?,
                to_perp: bool_f(m, "toPerp")?,
                nonce: u64_f(m, "nonce")?,
            }
        }
        "withdraw3" => Action::Withdraw3 {
            destination: addr_f(m, "destination")?,
            amount: parse_usd_amount(str_f(m, "amount")?)?,
            time: u64_f(m, "time")?,
        },
        "approveAgent" => {
            // Python omits agentName for unnamed agents (signing with ""),
            // the TS SDK sends "" (null is coerced to ""). All map to None.
            let agent_name = match opt_field(m, "agentName") {
                None => None,
                Some(v) => {
                    let s = as_str(&v, "agentName")?;
                    (!s.is_empty()).then(|| s.to_string())
                }
            };
            Action::ApproveAgent {
                agent_address: addr_f(m, "agentAddress")?,
                agent_name,
                nonce: u64_f(m, "nonce")?,
            }
        }
        "setReferrer" => Action::SetReferrer {
            code: str_f(m, "code")?.to_string(),
        },
        "noop" => Action::Noop,
        other => return Err(format!("unsupported action type: {other}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json_to_msgpack;
    use serde_json::json;

    fn dec(v: serde_json::Value) -> Result<Action, String> {
        decode_action(&json_to_msgpack(&v).unwrap())
    }

    #[test]
    fn decimals() {
        assert_eq!(parse_wire_decimal("1e-05", 6).unwrap(), 10);
        assert_eq!(parse_wire_decimal("1.5E3", 6).unwrap(), 1_500_000_000);
        assert_eq!(parse_wire_decimal("-2.5e-1", 6).unwrap(), -250_000);
        assert_eq!(parse_wire_decimal("12.50", 6).unwrap(), 12_500_000);
        assert_eq!(parse_wire_decimal("2.0", 6).unwrap(), 2_000_000);
        assert!(parse_wire_decimal("1e-07", 6).is_err());
        assert!(parse_wire_decimal("0.0000001", 6).is_err());
        assert!(parse_wire_decimal(" 1", 6).is_err());
        assert!(parse_wire_decimal("1e", 6).is_err());
        assert!(parse_wire_decimal("e5", 6).is_err());
        assert!(parse_wire_decimal("nan", 6).is_err());
        assert!(parse_wire_decimal("1e999", 6).is_err());
        assert!(parse_px("-1").is_err());
        assert!(parse_sz("-1").is_err());
        assert_eq!(parse_px("0.0001234").unwrap(), Px(12340));
        assert!(parse_px("0.000000001").is_err());
        assert!(parse_usd_amount("-1").is_err());
    }

    #[test]
    fn cloids_and_addresses() {
        let c = parse_cloid("0x00000000000000000000000000ABCDEF").unwrap();
        assert_eq!(cloid_to_hex(&c), "0x00000000000000000000000000abcdef");
        assert_eq!(c, cloid_from_u128(0xabcdef));
        assert!(parse_cloid("00000000000000000000000000ABCDEF").is_err());
        assert!(parse_cloid("0x0000").is_err());
        assert!(parse_cloid("0xzz000000000000000000000000ABCDEF").is_err());
        let a = parse_address("0x5E9EE1089755C3435139848E47E6635505D5A13A").unwrap();
        assert_eq!(a.to_hex(), "0x5e9ee1089755c3435139848e47e6635505d5a13a");
        assert!(parse_address("0x5e9e").is_err());
    }

    #[test]
    fn decode_variants() {
        let a = dec(json!({"type":"order","orders":[{"a":3,"b":true,"p":"1.5","s":"2","r":false,"t":{"trigger":{"isMarket":false,"triggerPx":"1.4","tpsl":"sl"}},"c":null}],"grouping":"positionTpsl"})).unwrap();
        match a {
            Action::Order { orders, grouping } => {
                assert_eq!(grouping, Grouping::PositionTpsl);
                assert_eq!(
                    orders[0].kind,
                    OrderKind::Trigger {
                        trigger_px: Px(140_000_000),
                        is_market: false,
                        tpsl: TpSl::Sl
                    }
                );
                assert_eq!(orders[0].cloid, None);
            }
            _ => panic!(),
        }
        assert_eq!(
            dec(json!({"type":"scheduleCancel","time":null})).unwrap(),
            Action::ScheduleCancel { time: None }
        );
        assert_eq!(dec(json!({"type":"noop"})).unwrap(), Action::Noop);
        assert_eq!(
            dec(json!({"type":"approveAgent","agentAddress":"0x5e9ee1089755c3435139848e47e6635505d5a13a","agentName":null,"nonce":5})).unwrap(),
            Action::ApproveAgent { agent_address: parse_address("0x5e9ee1089755c3435139848e47e6635505d5a13a").unwrap(), agent_name: None, nonce: 5 }
        );
        assert_eq!(
            dec(json!({"type":"modify","oid":"0x00000000000000000000000000000001","order":{"a":0,"b":true,"p":"1","s":"1","r":false,"t":{"limit":{"tif":"Gtc"}}}})).unwrap(),
            Action::Modify {
                oid: OidOrCloid::Cloid(cloid_from_u128(1)),
                order: OrderRequest { asset: 0, is_buy: true, limit_px: Px(100_000_000), sz: Sz(100_000_000), reduce_only: false, kind: OrderKind::Limit { tif: Tif::Gtc }, cloid: None }
            }
        );
    }

    #[test]
    fn decode_rejects() {
        let bad = [
            json!({"type":"twapOrder"}),
            json!({"orders":[]}),
            json!([1, 2]),
            json!({"type":"order","orders":[{"a":0,"b":true,"p":"1.000000001","s":"1","r":false,"t":{"limit":{"tif":"Gtc"}}}],"grouping":"na"}),
            json!({"type":"order","orders":[{"a":0,"b":true,"p":"1","s":"1","r":false,"t":{"limit":{"tif":"Fok"}}}],"grouping":"na"}),
            json!({"type":"order","orders":[{"a":0,"b":true,"p":1,"s":"1","r":false,"t":{"limit":{"tif":"Gtc"}}}],"grouping":"na"}),
            json!({"type":"order","orders":[{"a":-1,"b":true,"p":"1","s":"1","r":false,"t":{"limit":{"tif":"Gtc"}}}],"grouping":"na"}),
            json!({"type":"order","orders":[{"a":0,"b":true,"p":"1","s":"1","r":false,"t":{}}],"grouping":"na"}),
            json!({"type":"order","orders":[{"a":0,"b":true,"p":"1","s":"1","r":false,"t":{"limit":{"tif":"Gtc"}}}],"grouping":"bad"}),
            json!({"type":"order","orders":[{"a":4294967296u64,"b":true,"p":"1","s":"1","r":false,"t":{"limit":{"tif":"Gtc"}}}]}),
            json!({"type":"cancel","cancels":[{"a":0,"o":-5}]}),
            json!({"type":"cancelByCloid","cancels":[{"asset":0,"cloid":"0x01"}]}),
            json!({"type":"updateLeverage","asset":0,"isCross":"yes","leverage":3}),
            json!({"type":"usdClassTransfer","amount":"1 subaccount:0x1719884eb866cb12b2287399b15f7db5e7d775ea","toPerp":true,"nonce":1}),
            json!({"type":"usdSend","destination":"0x12","amount":"1","time":1}),
            json!({"type":"withdraw3","destination":"0x5e9ee1089755c3435139848e47e6635505d5a13a","amount":"-1","time":1}),
            json!({"type":"updateIsolatedMargin","asset":0,"isBuy":true,"ntli":1.5}),
        ];
        for b in bad {
            assert!(dec(b.clone()).is_err(), "{b}");
        }
    }
}
