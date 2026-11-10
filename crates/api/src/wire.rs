//! Bridge to `crates/hlwire`: `/exchange` JSON body -> [`UserTx`]
//! (action msgpack-encoded with key order preserved, as HL hashes it).
//!
//! [`parse_exchange_bytes`] has a zero-copy fast path that streams the action
//! JSON straight into msgpack (no `serde_json::Value`, which is slow with the
//! workspace's `preserve_order` + `arbitrary_precision` features). Anything
//! unusual (duplicate keys, odd numbers, malformed input, ...) falls back to
//! `hlwire::parse_exchange_request`, which is authoritative; the two are
//! differential-tested to produce identical `UserTx`s.

use interfaces::UserTx;
use serde::de::{DeserializeSeed, Deserializer, Error as _, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Value;
use std::borrow::Cow;
use std::fmt;

pub fn parse_exchange_request(body: &Value) -> Result<UserTx, String> {
    hlwire::parse_exchange_request(body)
}

/// Parse raw `/exchange` body bytes (fast path, hlwire fallback).
pub fn parse_exchange_bytes(bytes: &[u8]) -> Result<UserTx, String> {
    if let Some(u) = fast_parse(bytes) {
        return Ok(u);
    }
    let v: Value = serde_json::from_slice(bytes).map_err(|_| "Failed to deserialize the JSON body into the target type".to_string())?;
    hlwire::parse_exchange_request(&v)
}

#[derive(Deserialize)]
struct Body<'a> {
    #[serde(borrow)]
    action: &'a RawValue,
    nonce: u64,
    signature: Value,
    #[serde(rename = "vaultAddress", default)]
    vault_address: Option<String>,
    #[serde(rename = "expiresAfter", default)]
    expires_after: Option<u64>,
}

/// Fast path; `None` means "use the authoritative slow path".
pub fn fast_parse(bytes: &[u8]) -> Option<UserTx> {
    let b: Body = serde_json::from_slice(bytes).ok()?;
    let mut out = Vec::with_capacity(b.action.get().len());
    let mut de = serde_json::Deserializer::from_str(b.action.get());
    Enc { out: &mut out, top: true }.deserialize(&mut de).ok()?;
    de.end().ok()?;
    // action must be a map with a string "type" (hlwire's check)
    let a = b.action.get().trim_start();
    if !a.starts_with('{') {
        return None;
    }
    hlwire::action_type(&out).ok()?;
    let signature = hlwire::parse_signature(&b.signature).ok()?;
    let vault_address = match &b.vault_address {
        Some(s) => Some(hlwire::parse_address(s).ok()?),
        None => None,
    };
    Some(UserTx { action_bytes: out, nonce: b.nonce, signature, vault_address, expires_after: b.expires_after })
}

const NUMBER_TOKEN: &str = "$serde_json::private::Number";

struct Enc<'o> {
    out: &'o mut Vec<u8>,
    top: bool,
}

fn header(n: usize, fix: u8, fix_limit: usize, m16: u8, m32: u8) -> Option<Vec<u8>> {
    let mut h = Vec::with_capacity(5);
    if n < fix_limit {
        h.push(fix | n as u8);
    } else if n < 0x1_0000 {
        h.push(m16);
        h.extend_from_slice(&(n as u16).to_be_bytes());
    } else if n <= u32::MAX as usize {
        h.push(m32);
        h.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        return None;
    }
    Some(h)
}

fn write_str(out: &mut Vec<u8>, s: &str) {
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

fn write_uint(out: &mut Vec<u8>, u: u64) {
    if u < 128 {
        out.push(u as u8);
    } else if u < 0x100 {
        out.extend_from_slice(&[0xcc, u as u8]);
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

fn write_int(out: &mut Vec<u8>, i: i64) {
    if i >= 0 {
        write_uint(out, i as u64)
    } else if i >= -32 {
        out.push(i as i8 as u8)
    } else if i >= i8::MIN as i64 {
        out.extend_from_slice(&[0xd0, i as i8 as u8])
    } else if i >= i16::MIN as i64 {
        out.push(0xd1);
        out.extend_from_slice(&(i as i16).to_be_bytes());
    } else if i >= i32::MIN as i64 {
        out.push(0xd2);
        out.extend_from_slice(&(i as i32).to_be_bytes());
    } else {
        out.push(0xd3);
        out.extend_from_slice(&i.to_be_bytes());
    }
}

fn write_number_token(out: &mut Vec<u8>, t: &str) -> Option<()> {
    if t.bytes().any(|c| c == b'.' || c == b'e' || c == b'E') {
        let f: f64 = t.parse().ok()?;
        if !f.is_finite() {
            return None;
        }
        out.push(0xcb);
        out.extend_from_slice(&f.to_bits().to_be_bytes());
    } else if let Ok(u) = t.parse::<u64>() {
        write_uint(out, u);
    } else {
        write_int(out, t.parse::<i64>().ok()?);
    }
    Some(())
}

impl<'de, 'o> DeserializeSeed<'de> for Enc<'o> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

struct Key<'de>(Cow<'de, str>);
struct KeySeed;
impl<'de> DeserializeSeed<'de> for KeySeed {
    type Value = Key<'de>;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Key<'de>, D::Error> {
        d.deserialize_str(KeySeed)
    }
}
impl<'de> Visitor<'de> for KeySeed {
    type Value = Key<'de>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("string key")
    }
    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Key<'de>, E> {
        Ok(Key(Cow::Borrowed(v)))
    }
    fn visit_str<E>(self, v: &str) -> Result<Key<'de>, E> {
        Ok(Key(Cow::Owned(v.to_string())))
    }
    fn visit_string<E>(self, v: String) -> Result<Key<'de>, E> {
        Ok(Key(Cow::Owned(v)))
    }
}

struct Raw<'de>(Cow<'de, str>);
impl<'de> Deserialize<'de> for Raw<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        KeySeed.deserialize(d).map(|k| Raw(k.0))
    }
}

impl<'de, 'o> Visitor<'de> for Enc<'o> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("JSON value")
    }
    fn visit_unit<E>(self) -> Result<(), E> {
        self.out.push(0xc0);
        Ok(())
    }
    fn visit_bool<E>(self, v: bool) -> Result<(), E> {
        self.out.push(if v { 0xc3 } else { 0xc2 });
        Ok(())
    }
    fn visit_u64<E>(self, v: u64) -> Result<(), E> {
        write_uint(self.out, v);
        Ok(())
    }
    fn visit_i64<E>(self, v: i64) -> Result<(), E> {
        write_int(self.out, v);
        Ok(())
    }
    fn visit_f64<E: serde::de::Error>(self, _v: f64) -> Result<(), E> {
        Err(E::custom("float without token")) // fallback
    }
    fn visit_str<E>(self, v: &str) -> Result<(), E> {
        write_str(self.out, v);
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        if self.top {
            return Err(A::Error::custom("action must be an object"));
        }
        let start = self.out.len();
        let mut n = 0usize;
        while seq.next_element_seed(Enc { out: self.out, top: false })?.is_some() {
            n += 1;
        }
        let h = header(n, 0x90, 16, 0xdc, 0xdd).ok_or_else(|| A::Error::custom("too large"))?;
        self.out.splice(start..start, h);
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let start = self.out.len();
        let mut keys: Vec<Cow<'de, str>> = Vec::new();
        while let Some(Key(k)) = map.next_key_seed(KeySeed)? {
            if keys.is_empty() && k == NUMBER_TOKEN {
                if self.top {
                    return Err(A::Error::custom("action must be an object"));
                }
                let Raw(t) = map.next_value::<Raw>()?;
                write_number_token(self.out, &t).ok_or_else(|| A::Error::custom("bad number"))?;
                return Ok(());
            }
            if keys.iter().any(|x| *x == k) {
                return Err(A::Error::custom("duplicate key")); // fallback handles it
            }
            write_str(self.out, &k);
            keys.push(k);
            map.next_value_seed(Enc { out: self.out, top: false })?;
        }
        let h = header(keys.len(), 0x80, 16, 0xde, 0xdf).ok_or_else(|| A::Error::custom("too large"))?;
        self.out.splice(start..start, h);
        Ok(())
    }
}
