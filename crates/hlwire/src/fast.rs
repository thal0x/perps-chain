//! Allocation-free single-pass msgpack decoding for the hot action types
//! (`order`, `cancel`, `cancelByCloid`, `modify`, `batchModify`).
//!
//! Semantics are identical to the generic path in `decode.rs` (which remains
//! the reference implementation and is used for every other action type):
//! the whole input is structurally validated (bounds, UTF-8 of every string,
//! string map keys, no bin/ext, nesting <= 64, no trailing bytes), the first
//! occurrence of a duplicated key wins, unknown keys are ignored, and
//! decimals are parsed exactly (straight from the msgpack string bytes).
//! `tests/props.rs` checks fast == reference on random and mutated input.

use crate::decode::{parse_cloid_bytes, parse_px_bytes, parse_sz_bytes};
use interfaces::*;

type R<T> = Result<T, String>;

const MAX_DEPTH: usize = 64;
/// Top-level maps with more keys than this use the generic path.
const MAX_TOP_KEYS: usize = 16;

#[derive(Clone, Copy)]
pub(crate) struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}

fn eof() -> String {
    "msgpack: unexpected end of input".into()
}

fn kind_of(t: u8) -> &'static str {
    match t {
        0x00..=0x7f | 0xcc..=0xd3 | 0xe0..=0xff => "int",
        0x80..=0x8f | 0xde | 0xdf => "map",
        0x90..=0x9f | 0xdc | 0xdd => "array",
        0xa0..=0xbf | 0xd9..=0xdb => "string",
        0xc0 => "null",
        0xc2 | 0xc3 => "bool",
        0xca | 0xcb => "float",
        _ => "invalid",
    }
}

impl<'a> Cur<'a> {
    pub(crate) fn new(b: &'a [u8], p: usize) -> Self {
        Cur { b, p }
    }

    #[inline(always)]
    fn peek(&self) -> R<u8> {
        self.b.get(self.p).copied().ok_or_else(eof)
    }

    #[inline(always)]
    fn take(&mut self, n: usize) -> R<&'a [u8]> {
        let e = self.p.checked_add(n).filter(|&e| e <= self.b.len()).ok_or_else(eof)?;
        let s = &self.b[self.p..e];
        self.p = e;
        Ok(s)
    }

    #[inline(always)]
    fn be(&mut self, n: usize) -> R<usize> {
        Ok(self.take(n)?.iter().fold(0usize, |a, &x| (a << 8) | x as usize))
    }

    /// Map header; `Err(kind)` (without consuming) if not a map.
    #[inline]
    fn map_len(&mut self) -> Result<usize, &'static str> {
        let t = self.peek().map_err(|_| "end of input")?;
        let n = match t {
            0x80..=0x8f => {
                self.p += 1;
                return Ok((t & 0x0f) as usize);
            }
            0xde => 2,
            0xdf => 4,
            _ => return Err(kind_of(t)),
        };
        self.p += 1;
        self.be(n).map_err(|_| "end of input")
    }

    #[inline]
    fn arr_len(&mut self) -> Result<usize, &'static str> {
        let t = self.peek().map_err(|_| "end of input")?;
        let n = match t {
            0x90..=0x9f => {
                self.p += 1;
                return Ok((t & 0x0f) as usize);
            }
            0xdc => 2,
            0xdd => 4,
            _ => return Err(kind_of(t)),
        };
        self.p += 1;
        self.be(n).map_err(|_| "end of input")
    }

    /// String bytes (UTF-8 validated).
    #[inline(always)]
    fn str_bytes(&mut self) -> Result<&'a [u8], &'static str> {
        let t = self.peek().map_err(|_| "end of input")?;
        let n = match t {
            0xa0..=0xbf => {
                self.p += 1;
                (t & 0x1f) as usize
            }
            0xd9..=0xdb => {
                self.p += 1;
                self.be(1 << (t - 0xd9)).map_err(|_| "end of input")?
            }
            _ => return Err(kind_of(t)),
        };
        let s = self.take(n).map_err(|_| "end of input")?;
        // ASCII fast path; full check otherwise.
        if !s.is_ascii() && std::str::from_utf8(s).is_err() {
            return Err("invalid utf-8");
        }
        Ok(s)
    }

    /// Unsigned integer (any msgpack int encoding with a non-negative value).
    #[inline(always)]
    fn uint(&mut self) -> Result<u64, &'static str> {
        let t = self.peek().map_err(|_| "end of input")?;
        let v = match t {
            0x00..=0x7f => {
                self.p += 1;
                return Ok(t as u64);
            }
            0xcc..=0xcf => {
                self.p += 1;
                self.be(1 << (t - 0xcc)).map_err(|_| "end of input")? as u64
            }
            0xd0..=0xd3 => {
                self.p += 1;
                let w = 1 << (t - 0xd0);
                let raw = self.be(w).map_err(|_| "end of input")? as u64;
                let shift = 64 - 8 * w as u32;
                let i = ((raw << shift) as i64) >> shift;
                if i < 0 {
                    return Err("int");
                }
                i as u64
            }
            _ => return Err(kind_of(t)),
        };
        Ok(v)
    }

    #[inline]
    fn boolean(&mut self) -> Result<bool, &'static str> {
        match self.peek().map_err(|_| "end of input")? {
            0xc2 => {
                self.p += 1;
                Ok(false)
            }
            0xc3 => {
                self.p += 1;
                Ok(true)
            }
            t => Err(kind_of(t)),
        }
    }

    #[inline]
    fn is_nil(&self) -> bool {
        self.b.get(self.p) == Some(&0xc0)
    }

    /// Skip one value at nesting `depth`, validating it exactly like the
    /// generic reader (bounds, UTF-8, string keys, supported types, depth).
    fn skip(&mut self, depth: usize) -> R<()> {
        if depth > MAX_DEPTH {
            return Err("msgpack: nesting too deep".into());
        }
        let t = self.peek()?;
        match t {
            0x00..=0x7f | 0xe0..=0xff | 0xc0 | 0xc2 | 0xc3 => {
                self.p += 1;
            }
            0xa0..=0xbf | 0xd9..=0xdb => {
                self.str_bytes().map_err(|e| format!("msgpack: {e}"))?;
            }
            0xcc..=0xcf => {
                self.p += 1;
                self.take(1 << (t - 0xcc))?;
            }
            0xd0..=0xd3 => {
                self.p += 1;
                self.take(1 << (t - 0xd0))?;
            }
            0xca => {
                self.p += 1;
                self.take(4)?;
            }
            0xcb => {
                self.p += 1;
                self.take(8)?;
            }
            0x80..=0x8f | 0xde | 0xdf => {
                let n = self.map_len().map_err(|e| format!("msgpack: {e}"))?;
                if n.saturating_mul(2) > self.b.len() - self.p {
                    return Err("msgpack: map length exceeds input".into());
                }
                for _ in 0..n {
                    if depth + 1 > MAX_DEPTH {
                        return Err("msgpack: nesting too deep".into());
                    }
                    match self.str_bytes() {
                        Ok(_) => {}
                        Err(e @ ("invalid utf-8" | "end of input")) => return Err(format!("msgpack: {e}")),
                        Err(k) => return Err(format!("msgpack: map key must be a string, got {k}")),
                    }
                    self.skip(depth + 1)?;
                }
            }
            0x90..=0x9f | 0xdc | 0xdd => {
                let n = self.arr_len().map_err(|e| format!("msgpack: {e}"))?;
                if n > self.b.len() - self.p {
                    return Err("msgpack: array length exceeds input".into());
                }
                for _ in 0..n {
                    self.skip(depth + 1)?;
                }
            }
            0xc4..=0xc6 => return Err("msgpack: binary values are not supported".into()),
            0xc7..=0xc9 | 0xd4..=0xd8 => return Err("msgpack: extension types are not supported".into()),
            _ => return Err("msgpack: reserved byte 0xc1".into()),
        }
        Ok(())
    }
}

fn field_err(k: &str, what: &str, got: &str) -> String {
    format!("field `{k}`: expected {what}, got {got}")
}

/// Record `(key, value offset)` for the entries of a map body with `n`
/// entries (first occurrence wins), skipping (and validating) values.
/// Returns the number of distinct keys stored.
fn index_map<'a, const N: usize>(
    c: &mut Cur<'a>,
    n: usize,
    depth: usize,
    want: &[&[u8]; N],
    out: &mut [Option<usize>; N],
) -> R<()> {
    for _ in 0..n {
        let k = c.str_bytes().map_err(|e| match e {
            "invalid utf-8" | "end of input" => format!("msgpack: {e}"),
            other => format!("msgpack: map key must be a string, got {other}"),
        })?;
        let pos = c.p;
        if let Some(i) = want.iter().position(|w| *w == k) {
            if out[i].is_none() {
                out[i] = Some(pos);
            }
        }
        c.skip(depth + 1)?;
    }
    Ok(())
}

/// Outcome of the fast path.
pub(crate) enum Fast {
    Done(Result<Action, String>),
    /// Not a hot action type (or unusual shape): use the generic decoder.
    Fallback,
}

pub(crate) fn decode_fast(bytes: &[u8]) -> Fast {
    let mut c = Cur::new(bytes, 0);
    let n = match c.map_len() {
        Ok(n) => n,
        Err(_) => return Fast::Fallback,
    };
    if n > MAX_TOP_KEYS || n.saturating_mul(2) > bytes.len() - c.p {
        return Fast::Fallback;
    }
    // Both SDKs put "type" first for L1 actions, so the payload array is
    // normally decoded inline in this single pass; otherwise it is decoded
    // from its recorded offset after the (validating) scan.
    const KEYS: [&[u8]; 7] = [b"type", b"orders", b"grouping", b"cancels", b"oid", b"order", b"modifies"];
    let mut off: [Option<usize>; 7] = [None; 7];
    let mut ty: Option<&[u8]> = None;
    let mut inline: Option<R<Action>> = None;
    for _ in 0..n {
        let k = match c.str_bytes() {
            Ok(k) => k,
            Err(e @ ("invalid utf-8" | "end of input")) => return Fast::Done(Err(format!("msgpack: {e}"))),
            Err(e) => return Fast::Done(Err(format!("msgpack: map key must be a string, got {e}"))),
        };
        let Some(i) = KEYS.iter().position(|w| *w == k) else {
            if let Err(e) = c.skip(1) {
                return Fast::Done(Err(e));
            }
            continue;
        };
        if off[i].is_some() {
            if let Err(e) = c.skip(1) {
                return Fast::Done(Err(e));
            }
            continue;
        }
        off[i] = Some(c.p);
        if i == 0 {
            ty = Cur::new(bytes, c.p).str_bytes().ok();
        }
        let payload = match (ty, i) {
            (Some(b"order"), 1) => Some(orders(&mut c).map(|o| Action::Order { orders: o, grouping: Grouping::Na })),
            (Some(b"cancel"), 3) => Some(cancels(&mut c).map(|cancels| Action::Cancel { cancels })),
            (Some(b"cancelByCloid"), 3) => Some(cancels_by_cloid(&mut c).map(|cancels| Action::CancelByCloid { cancels })),
            (Some(b"batchModify"), 6) => Some(modifies(&mut c).map(|modifies| Action::BatchModify { modifies })),
            _ => None,
        };
        match payload {
            Some(Err(e)) => return Fast::Done(Err(e)),
            Some(Ok(a)) => inline = Some(Ok(a)),
            None => {
                if let Err(e) = c.skip(1) {
                    return Fast::Done(Err(e));
                }
            }
        }
    }
    if c.p != bytes.len() {
        return Fast::Done(Err(format!("msgpack: {} trailing bytes", bytes.len() - c.p)));
    }
    // Input fully validated from here on.
    let Some(t) = ty else { return Fast::Fallback };
    let at = |i: usize| off[i].map(|p| Cur::new(bytes, p));
    let miss = |k: &str| Err(format!("missing field `{k}`"));
    let r = match t {
        b"order" => {
            let base = match inline {
                Some(r) => r,
                None => match at(1) {
                    None => miss("orders"),
                    Some(mut o) => orders(&mut o).map(|o| Action::Order { orders: o, grouping: Grouping::Na }),
                },
            };
            base.and_then(|a| {
                let Action::Order { orders, .. } = a else { unreachable!() };
                let grouping = match at(2) {
                    None => Grouping::Na,
                    Some(g) => grouping(g)?,
                };
                Ok(Action::Order { orders, grouping })
            })
        }
        b"cancel" => inline.unwrap_or_else(|| match at(3) {
            None => miss("cancels"),
            Some(mut x) => cancels(&mut x).map(|cancels| Action::Cancel { cancels }),
        }),
        b"cancelByCloid" => inline.unwrap_or_else(|| match at(3) {
            None => miss("cancels"),
            Some(mut x) => cancels_by_cloid(&mut x).map(|cancels| Action::CancelByCloid { cancels }),
        }),
        b"modify" => match (at(4), at(5)) {
            (None, _) => miss("oid"),
            (_, None) => miss("order"),
            (Some(mut oc), Some(mut or)) => (|| {
                let oid = oid_or_cloid(&mut oc)?;
                let order = order_value(&mut or, 1)?;
                Ok(Action::Modify { oid, order })
            })(),
        },
        b"batchModify" => inline.unwrap_or_else(|| match at(6) {
            None => miss("modifies"),
            Some(mut x) => modifies(&mut x).map(|modifies| Action::BatchModify { modifies }),
        }),
        _ => return Fast::Fallback,
    };
    Fast::Done(r)
}

fn grouping(mut g: Cur<'_>) -> R<Grouping> {
    if g.is_nil() {
        return Ok(Grouping::Na);
    }
    match g.str_bytes() {
        Ok(b"na") => Ok(Grouping::Na),
        Ok(b"normalTpsl") => Ok(Grouping::NormalTpsl),
        Ok(b"positionTpsl") => Ok(Grouping::PositionTpsl),
        Ok(s) => Err(format!("invalid grouping: {}", String::from_utf8_lossy(s))),
        Err("map") => {
            // Priority grouping {p: n} (TS SDK): priority fees are not modeled.
            let n = g.map_len().map_err(|e| e.to_string())?;
            let mut off = [None; 1];
            index_map(&mut g, n, 1, &[b"p"], &mut off)?;
            match off[0] {
                None => Err("missing field `p`".into()),
                Some(p) => Cur::new(g.b, p).uint().map(|_| Grouping::Na).map_err(|e| field_err("p", "unsigned integer", e)),
            }
        }
        Err(k) => Err(format!("invalid grouping: {k}")),
    }
}

/// Array header, bounded by the remaining input (no allocation bombs).
fn arr(c: &mut Cur<'_>, k: &str) -> R<usize> {
    let n = c.arr_len().map_err(|e| field_err(k, "array", e))?;
    if n > c.b.len() - c.p {
        return Err("msgpack: array length exceeds input".into());
    }
    Ok(n)
}

fn orders(c: &mut Cur<'_>) -> R<Vec<OrderRequest>> {
    let n = arr(c, "orders")?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(order_value(c, 2)?);
    }
    Ok(out)
}

/// An OrderWire map `{a,b,p,s,r,t,c?}` at nesting `depth`, single pass.
fn order_value(c: &mut Cur<'_>, depth: usize) -> R<OrderRequest> {
    let n = c.map_len().map_err(|e| format!("order: expected map, got {e}"))?;
    let (mut a, mut b, mut p, mut s, mut r, mut t, mut cl): (_, _, _, _, _, _, Option<Option<Cloid>>) =
        (None, None, None, None, None, None, None);
    for _ in 0..n {
        let k = c.str_bytes().map_err(|e| format!("msgpack: map key must be a string, got {e}"))?;
        match k {
            b"a" if a.is_none() => {
                let v = c.uint().map_err(|e| field_err("a", "unsigned integer", e))?;
                a = Some(u32::try_from(v).map_err(|_| format!("field `a`: value {v} out of range"))?);
            }
            b"b" if b.is_none() => b = Some(c.boolean().map_err(|e| field_err("b", "bool", e))?),
            b"p" if p.is_none() => p = Some(parse_px_bytes(c.str_bytes().map_err(|e| field_err("p", "string", e))?)?),
            b"s" if s.is_none() => s = Some(parse_sz_bytes(c.str_bytes().map_err(|e| field_err("s", "string", e))?)?),
            b"r" if r.is_none() => r = Some(c.boolean().map_err(|e| field_err("r", "bool", e))?),
            b"t" if t.is_none() => t = Some(order_type(c, depth + 1)?),
            b"c" if cl.is_none() => {
                cl = Some(if c.is_nil() {
                    c.p += 1;
                    None
                } else {
                    Some(parse_cloid_bytes(c.str_bytes().map_err(|e| field_err("c", "string", e))?)?)
                });
            }
            _ => c.skip(depth + 1)?,
        }
    }
    let miss = |k: &str| format!("missing field `{k}`");
    Ok(OrderRequest {
        asset: a.ok_or_else(|| miss("a"))?,
        is_buy: b.ok_or_else(|| miss("b"))?,
        limit_px: p.ok_or_else(|| miss("p"))?,
        sz: s.ok_or_else(|| miss("s"))?,
        reduce_only: r.ok_or_else(|| miss("r"))?,
        kind: t.ok_or_else(|| miss("t"))?,
        cloid: cl.flatten(),
    })
}

fn order_type(c: &mut Cur<'_>, depth: usize) -> R<OrderKind> {
    let n = c.map_len().map_err(|e| format!("order type `t`: expected map, got {e}"))?;
    let mut off = [None; 2];
    index_map(c, n, depth, &[b"limit", b"trigger"], &mut off)?;
    let present = |o: Option<usize>| o.filter(|&p| c.b[p] != 0xc0);
    if let Some(p) = present(off[0]) {
        let mut l = Cur::new(c.b, p);
        let ln = l.map_len().map_err(|_| "missing field `tif`".to_string())?;
        let mut lo = [None; 1];
        index_map(&mut l, ln, depth + 1, &[b"tif"], &mut lo)?;
        let tif = Cur::new(c.b, lo[0].ok_or("missing field `tif`")?).str_bytes().map_err(|e| field_err("tif", "string", e))?;
        let tif = match tif {
            b"Gtc" => Tif::Gtc,
            b"Ioc" => Tif::Ioc,
            b"Alo" => Tif::Alo,
            // TS SDK: "Similar to Ioc, but add a note that this is market order".
            b"FrontendMarket" => Tif::Ioc,
            other => return Err(format!("invalid tif: {}", String::from_utf8_lossy(other))),
        };
        Ok(OrderKind::Limit { tif })
    } else if let Some(p) = present(off[1]) {
        let mut tr = Cur::new(c.b, p);
        let tn = tr.map_len().map_err(|_| "missing field `isMarket`".to_string())?;
        let mut to = [None; 3];
        index_map(&mut tr, tn, depth + 1, &[b"isMarket", b"triggerPx", b"tpsl"], &mut to)?;
        let at = |i: usize, k: &str| to[i].map(|p| Cur::new(c.b, p)).ok_or_else(|| format!("missing field `{k}`"));
        let is_market = at(0, "isMarket")?.boolean().map_err(|e| field_err("isMarket", "bool", e))?;
        let trigger_px = parse_px_bytes(at(1, "triggerPx")?.str_bytes().map_err(|e| field_err("triggerPx", "string", e))?)?;
        let tpsl = match at(2, "tpsl")?.str_bytes().map_err(|e| field_err("tpsl", "string", e))? {
            b"tp" => TpSl::Tp,
            b"sl" => TpSl::Sl,
            other => return Err(format!("invalid tpsl: {}", String::from_utf8_lossy(other))),
        };
        Ok(OrderKind::Trigger { trigger_px, is_market, tpsl })
    } else {
        Err("order type must be `limit` or `trigger`".into())
    }
}

fn cancels(c: &mut Cur<'_>) -> R<Vec<(AssetId, Oid)>> {
    let n = arr(c, "cancels")?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let m = c.map_len().map_err(|e| format!("cancel: expected map, got {e}"))?;
        let (mut a, mut o) = (None, None);
        for _ in 0..m {
            match c.str_bytes().map_err(|e| format!("msgpack: map key must be a string, got {e}"))? {
                b"a" if a.is_none() => {
                    let v = c.uint().map_err(|e| field_err("a", "unsigned integer", e))?;
                    a = Some(u32::try_from(v).map_err(|_| format!("field `a`: value {v} out of range"))?);
                }
                b"o" if o.is_none() => o = Some(c.uint().map_err(|e| field_err("o", "unsigned integer", e))?),
                _ => c.skip(3)?,
            }
        }
        out.push((a.ok_or("missing field `a`")?, o.ok_or("missing field `o`")?));
    }
    Ok(out)
}

fn cancels_by_cloid(c: &mut Cur<'_>) -> R<Vec<(AssetId, Cloid)>> {
    let n = arr(c, "cancels")?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let m = c.map_len().map_err(|e| format!("cancel: expected map, got {e}"))?;
        let (mut a, mut cl) = (None, None);
        for _ in 0..m {
            match c.str_bytes().map_err(|e| format!("msgpack: map key must be a string, got {e}"))? {
                b"asset" if a.is_none() => {
                    let v = c.uint().map_err(|e| field_err("asset", "unsigned integer", e))?;
                    a = Some(u32::try_from(v).map_err(|_| format!("field `asset`: value {v} out of range"))?);
                }
                b"cloid" if cl.is_none() => {
                    cl = Some(parse_cloid_bytes(c.str_bytes().map_err(|e| field_err("cloid", "string", e))?)?)
                }
                _ => c.skip(3)?,
            }
        }
        out.push((a.ok_or("missing field `asset`")?, cl.ok_or("missing field `cloid`")?));
    }
    Ok(out)
}

fn oid_or_cloid(c: &mut Cur<'_>) -> R<OidOrCloid> {
    let t = c.peek()?;
    match kind_of(t) {
        "string" => Ok(OidOrCloid::Cloid(parse_cloid_bytes(c.str_bytes().map_err(|e| e.to_string())?)?)),
        "int" => c.uint().map(OidOrCloid::Oid).map_err(|e| format!("field `oid`: expected integer or cloid string, got {e}")),
        k => Err(format!("field `oid`: expected integer or cloid string, got {k}")),
    }
}

fn modifies(c: &mut Cur<'_>) -> R<Vec<(OidOrCloid, OrderRequest)>> {
    let n = arr(c, "modifies")?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let m = c.map_len().map_err(|e| format!("modify: expected map, got {e}"))?;
        let (mut oid, mut order) = (None, None);
        for _ in 0..m {
            match c.str_bytes().map_err(|e| format!("msgpack: map key must be a string, got {e}"))? {
                b"oid" if oid.is_none() => oid = Some(oid_or_cloid(c)?),
                b"order" if order.is_none() => order = Some(order_value(c, 3)?),
                _ => c.skip(3)?,
            }
        }
        out.push((oid.ok_or("missing field `oid`")?, order.ok_or("missing field `order`")?));
    }
    Ok(out)
}
