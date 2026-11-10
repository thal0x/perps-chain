//! MessagePack encoding of HL action JSON, byte-identical to Python
//! `msgpack.packb(obj)` (msgpack >= 1.0 defaults) and to `@std/msgpack`
//! `encode` as used by the TS SDK.
//!
//! Rules (identical in both SDKs for every value an HL action can contain):
//! * maps keep insertion order (serde_json `preserve_order`), keys are str;
//! * integers use the smallest encoding: positive fixint / uint8 / uint16 /
//!   uint32 / uint64, negative fixint / int8 / int16 / int32 / int64;
//! * a JSON number written with `.`, `e` or `E` is a float and is encoded as
//!   float64 (Python semantics: `json.loads("1.0")` is a float);
//! * strings: fixstr (< 32 bytes) / str8 / str16 / str32;
//! * arrays: fixarray (< 16) / array16 / array32; maps: fixmap / map16 / map32;
//! * null -> nil, booleans -> true/false.

use serde_json::{Map, Number, Value};

/// Encode a JSON value as msgpack exactly like the SDKs do before hashing.
pub fn json_to_msgpack(v: &Value) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(128);
    write_value(&mut out, v)?;
    Ok(out)
}

/// Alias used by `interfaces::tx` docs.
pub fn action_to_msgpack(v: &Value) -> Result<Vec<u8>, String> {
    json_to_msgpack(v)
}

fn write_value(out: &mut Vec<u8>, v: &Value) -> Result<(), String> {
    match v {
        Value::Null => out.push(0xc0),
        Value::Bool(false) => out.push(0xc2),
        Value::Bool(true) => out.push(0xc3),
        Value::Number(n) => write_number(out, n)?,
        Value::String(s) => write_str(out, s),
        Value::Array(a) => {
            write_len(out, a.len(), 0x90, 16, 0xdc, 0xdd)?;
            for x in a {
                write_value(out, x)?;
            }
        }
        Value::Object(m) => {
            write_len(out, m.len(), 0x80, 16, 0xde, 0xdf)?;
            for (k, x) in m {
                write_str(out, k);
                write_value(out, x)?;
            }
        }
    }
    Ok(())
}

fn write_len(out: &mut Vec<u8>, n: usize, fix: u8, fix_limit: usize, m16: u8, m32: u8) -> Result<(), String> {
    if n < fix_limit {
        out.push(fix | n as u8);
    } else if n < 0x1_0000 {
        out.push(m16);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if n <= u32::MAX as usize {
        out.push(m32);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        return Err("msgpack: container too large".into());
    }
    Ok(())
}

pub(crate) fn write_str(out: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    let n = b.len();
    if n < 32 {
        out.push(0xa0 | n as u8);
    } else if n < 0x100 {
        out.push(0xd9);
        out.push(n as u8);
    } else if n < 0x1_0000 {
        out.push(0xda);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        out.push(0xdb);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    }
    out.extend_from_slice(b);
}

/// True if the JSON number token is a float literal.
fn is_float_token(n: &Number) -> bool {
    // With `arbitrary_precision` the Display impl yields the original token.
    let s = n.to_string();
    s.bytes().any(|c| c == b'.' || c == b'e' || c == b'E')
}

fn write_number(out: &mut Vec<u8>, n: &Number) -> Result<(), String> {
    if is_float_token(n) {
        let f = n.as_f64().ok_or_else(|| format!("msgpack: bad float {n}"))?;
        if !f.is_finite() {
            return Err(format!("msgpack: non-finite float {n}"));
        }
        out.push(0xcb);
        out.extend_from_slice(&f.to_bits().to_be_bytes());
        return Ok(());
    }
    if let Some(u) = n.as_u64() {
        write_uint(out, u);
        Ok(())
    } else if let Some(i) = n.as_i64() {
        write_int(out, i);
        Ok(())
    } else {
        Err(format!("msgpack: integer out of 64-bit range: {n}"))
    }
}

pub(crate) fn write_uint(out: &mut Vec<u8>, u: u64) {
    if u < 0x80 {
        out.push(u as u8);
    } else if u < 0x100 {
        out.push(0xcc);
        out.push(u as u8);
    } else if u < 0x1_0000 {
        out.push(0xcd);
        out.extend_from_slice(&(u as u16).to_be_bytes());
    } else if u < 0x1_0000_0000 {
        out.push(0xce);
        out.extend_from_slice(&(u as u32).to_be_bytes());
    } else {
        out.push(0xcf);
        out.extend_from_slice(&u.to_be_bytes());
    }
}

pub(crate) fn write_int(out: &mut Vec<u8>, i: i64) {
    if i >= 0 {
        write_uint(out, i as u64);
    } else if i >= -32 {
        out.push(i as i8 as u8);
    } else if i >= -128 {
        out.push(0xd0);
        out.push(i as i8 as u8);
    } else if i >= -32768 {
        out.push(0xd1);
        out.extend_from_slice(&(i as i16).to_be_bytes());
    } else if i >= -(1i64 << 31) {
        out.push(0xd2);
        out.extend_from_slice(&(i as i32).to_be_bytes());
    } else {
        out.push(0xd3);
        out.extend_from_slice(&i.to_be_bytes());
    }
}

// ------------------------------------------------------------------ decoding

/// Borrowed msgpack value tree (cheap to build; used by `decode_action`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Mp<'a> {
    Nil,
    Bool(bool),
    UInt(u64),
    /// Always negative.
    NegInt(i64),
    /// Raw IEEE-754 bits of a float32/float64 (never used for arithmetic).
    Float(u64),
    Str(&'a str),
    Arr(Vec<Mp<'a>>),
    Map(Vec<(&'a str, Mp<'a>)>),
}

impl<'a> Mp<'a> {
    pub(crate) fn get(&self, key: &str) -> Option<&Mp<'a>> {
        match self {
            Mp::Map(m) => m.iter().find(|(k, _)| *k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Mp::Nil => "null",
            Mp::Bool(_) => "bool",
            Mp::UInt(_) | Mp::NegInt(_) => "int",
            Mp::Float(_) => "float",
            Mp::Str(_) => "string",
            Mp::Arr(_) => "array",
            Mp::Map(_) => "map",
        }
    }
}

const MAX_DEPTH: usize = 64;

pub(crate) struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Reader { b, pos: 0 }
    }

    /// Parse exactly one value spanning the whole input.
    pub(crate) fn parse_all(b: &'a [u8]) -> Result<Mp<'a>, String> {
        let mut r = Reader::new(b);
        let v = r.value(0)?;
        if r.pos != b.len() {
            return Err(format!("msgpack: {} trailing bytes", b.len() - r.pos));
        }
        Ok(v)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).ok_or("msgpack: length overflow")?;
        if end > self.b.len() {
            return Err("msgpack: unexpected end of input".into());
        }
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn be<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }
    fn str(&mut self, n: usize) -> Result<&'a str, String> {
        let s = self.take(n)?;
        std::str::from_utf8(s).map_err(|_| "msgpack: invalid utf-8 string".to_string())
    }
    fn neg(i: i64) -> Mp<'a> {
        if i >= 0 {
            Mp::UInt(i as u64)
        } else {
            Mp::NegInt(i)
        }
    }

    fn value(&mut self, depth: usize) -> Result<Mp<'a>, String> {
        if depth > MAX_DEPTH {
            return Err("msgpack: nesting too deep".into());
        }
        let t = self.u8()?;
        Ok(match t {
            0x00..=0x7f => Mp::UInt(t as u64),
            0x80..=0x8f => self.map((t & 0x0f) as usize, depth)?,
            0x90..=0x9f => self.arr((t & 0x0f) as usize, depth)?,
            0xa0..=0xbf => Mp::Str(self.str((t & 0x1f) as usize)?),
            0xc0 => Mp::Nil,
            0xc2 => Mp::Bool(false),
            0xc3 => Mp::Bool(true),
            0xca => Mp::Float((f32::from_bits(u32::from_be_bytes(self.be()?)) as f64).to_bits()),
            0xcb => Mp::Float(u64::from_be_bytes(self.be()?)),
            0xcc => Mp::UInt(self.u8()? as u64),
            0xcd => Mp::UInt(u16::from_be_bytes(self.be()?) as u64),
            0xce => Mp::UInt(u32::from_be_bytes(self.be()?) as u64),
            0xcf => Mp::UInt(u64::from_be_bytes(self.be()?)),
            0xd0 => Self::neg(self.u8()? as i8 as i64),
            0xd1 => Self::neg(i16::from_be_bytes(self.be()?) as i64),
            0xd2 => Self::neg(i32::from_be_bytes(self.be()?) as i64),
            0xd3 => Self::neg(i64::from_be_bytes(self.be()?)),
            0xd9 => {
                let n = self.u8()? as usize;
                Mp::Str(self.str(n)?)
            }
            0xda => {
                let n = u16::from_be_bytes(self.be()?) as usize;
                Mp::Str(self.str(n)?)
            }
            0xdb => {
                let n = u32::from_be_bytes(self.be()?) as usize;
                Mp::Str(self.str(n)?)
            }
            0xdc => {
                let n = u16::from_be_bytes(self.be()?) as usize;
                self.arr(n, depth)?
            }
            0xdd => {
                let n = u32::from_be_bytes(self.be()?) as usize;
                self.arr(n, depth)?
            }
            0xde => {
                let n = u16::from_be_bytes(self.be()?) as usize;
                self.map(n, depth)?
            }
            0xdf => {
                let n = u32::from_be_bytes(self.be()?) as usize;
                self.map(n, depth)?
            }
            0xe0..=0xff => Mp::NegInt(t as i8 as i64),
            0xc4..=0xc6 => return Err("msgpack: binary values are not supported".into()),
            0xc7..=0xc9 | 0xd4..=0xd8 => return Err("msgpack: extension types are not supported".into()),
            0xc1 => return Err("msgpack: reserved byte 0xc1".into()),
        })
    }

    fn arr(&mut self, n: usize, depth: usize) -> Result<Mp<'a>, String> {
        // Each element needs at least one byte: bound allocation by input size.
        if n > self.b.len() - self.pos {
            return Err("msgpack: array length exceeds input".into());
        }
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(self.value(depth + 1)?);
        }
        Ok(Mp::Arr(v))
    }

    fn map(&mut self, n: usize, depth: usize) -> Result<Mp<'a>, String> {
        if n.saturating_mul(2) > self.b.len() - self.pos {
            return Err("msgpack: map length exceeds input".into());
        }
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let k = match self.value(depth + 1)? {
                Mp::Str(s) => s,
                other => return Err(format!("msgpack: map key must be a string, got {}", other.kind())),
            };
            let x = self.value(depth + 1)?;
            v.push((k, x));
        }
        Ok(Mp::Map(v))
    }
}

fn mp_to_json(v: &Mp<'_>) -> Result<Value, String> {
    Ok(match v {
        Mp::Nil => Value::Null,
        Mp::Bool(b) => Value::Bool(*b),
        Mp::UInt(u) => Value::Number((*u).into()),
        Mp::NegInt(i) => Value::Number((*i).into()),
        Mp::Float(bits) => {
            let f = f64::from_bits(*bits);
            Value::Number(Number::from_f64(f).ok_or("msgpack: non-finite float")?)
        }
        Mp::Str(s) => Value::String((*s).to_string()),
        Mp::Arr(a) => Value::Array(a.iter().map(mp_to_json).collect::<Result<_, _>>()?),
        Mp::Map(m) => {
            let mut o = Map::with_capacity(m.len());
            for (k, x) in m {
                o.insert((*k).to_string(), mp_to_json(x)?);
            }
            Value::Object(o)
        }
    })
}

/// Decode msgpack bytes (as produced by [`json_to_msgpack`]) back into JSON,
/// preserving map order. Binary/ext values and non-string keys are rejected.
/// Floats come back as JSON float numbers (always containing `.` or `e`), so
/// `json_to_msgpack(msgpack_to_json(b)) == b` for canonical encodings.
pub fn msgpack_to_json(b: &[u8]) -> Result<Value, String> {
    let v = Reader::parse_all(b)?;
    mp_to_json(&v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hx(v: &Value) -> String {
        hex::encode(json_to_msgpack(v).unwrap())
    }

    #[test]
    fn ints_smallest_encoding() {
        let cases: &[(&str, &str)] = &[
            ("0", "00"),
            ("127", "7f"),
            ("128", "cc80"),
            ("255", "ccff"),
            ("256", "cd0100"),
            ("65535", "cdffff"),
            ("65536", "ce00010000"),
            ("4294967295", "ceffffffff"),
            ("4294967296", "cf0000000100000000"),
            ("18446744073709551615", "cfffffffffffffffff"),
            ("-1", "ff"),
            ("-32", "e0"),
            ("-33", "d0df"),
            ("-128", "d080"),
            ("-129", "d1ff7f"),
            ("-32768", "d18000"),
            ("-32769", "d2ffff7fff"),
            ("-2147483648", "d280000000"),
            ("-2147483649", "d3ffffffff7fffffff"),
            ("-0", "00"),
        ];
        for (j, h) in cases {
            let v: Value = serde_json::from_str(j).unwrap();
            assert_eq!(hx(&v), *h, "{j}");
        }
        assert!(json_to_msgpack(&serde_json::from_str::<Value>("18446744073709551616").unwrap()).is_err());
        assert!(json_to_msgpack(&serde_json::from_str::<Value>("-9223372036854775809").unwrap()).is_err());
    }

    #[test]
    fn floats_are_float64() {
        let v: Value = serde_json::from_str("1.0").unwrap();
        assert_eq!(hx(&v), "cb3ff0000000000000");
        let v: Value = serde_json::from_str("1e3").unwrap();
        assert_eq!(hx(&v), "cb408f400000000000");
        let v: Value = serde_json::from_str("-0.5").unwrap();
        assert_eq!(hx(&v), "cbbfe0000000000000");
    }

    #[test]
    fn strings_and_containers() {
        assert_eq!(hx(&json!("")), "a0");
        assert_eq!(hx(&json!("a".repeat(31))), format!("bf{}", "61".repeat(31)));
        assert_eq!(hx(&json!("a".repeat(32))), format!("d920{}", "61".repeat(32)));
        assert_eq!(hx(&json!("a".repeat(256)))[..6], *"da0100");
        assert_eq!(hx(&json!([])), "90");
        assert_eq!(hx(&json!({})), "80");
        assert_eq!(hx(&json!(null)), "c0");
        assert_eq!(hx(&json!([true, false])), "92c3c2");
        let arr: Vec<u32> = (0..16).collect();
        assert!(hx(&json!(arr)).starts_with("dc0010"));
        // insertion order kept
        let v: Value = serde_json::from_str(r#"{"b":1,"a":2}"#).unwrap();
        assert_eq!(hx(&v), "82a16201a16102");
    }

    #[test]
    fn roundtrip_and_rejects() {
        let v: Value =
            serde_json::from_str(r#"{"type":"x","n":[1,-1,300,-300,70000,-70000,5000000000,-5000000000,1.5],"s":null,"b":true}"#)
                .unwrap();
        let b = json_to_msgpack(&v).unwrap();
        let back = msgpack_to_json(&b).unwrap();
        assert_eq!(json_to_msgpack(&back).unwrap(), b);
        assert_eq!(back.to_string(), v.to_string());
        assert!(msgpack_to_json(&[0xc4, 0x01, 0x00]).is_err());
        assert!(msgpack_to_json(&[0x81, 0x01, 0x01]).is_err());
        assert!(msgpack_to_json(&[0x92, 0x01]).is_err());
        assert!(msgpack_to_json(&[0x01, 0x01]).is_err());
        assert!(msgpack_to_json(&[0xdd, 0xff, 0xff, 0xff, 0xff]).is_err());
        assert!(msgpack_to_json(&[0xc1]).is_err());
        // deep nesting
        let deep = vec![0x91u8; 100];
        assert!(msgpack_to_json(&deep).is_err());
    }
}
