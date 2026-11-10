//! Bridge to `crates/hlwire`.
//!
//! `parse_exchange_request` turns a `/exchange` JSON body into a [`UserTx`].
//! Until `hlwire` lands this is a local implementation (msgpack encoding
//! preserving key order, as HL hashes it); once `hlwire::parse_exchange_request`
//! exists this module should just re-export it (delete `local`).

use interfaces::*;
use serde_json::Value;

pub fn parse_exchange_request(body: &Value) -> Result<UserTx, String> {
    local::parse_exchange_request(body)
}

pub mod local {
    use super::*;

    pub fn parse_exchange_request(body: &Value) -> Result<UserTx, String> {
        let obj = body.as_object().ok_or("Failed to deserialize the JSON body into the target type")?;
        let action = obj.get("action").ok_or("Failed to deserialize the JSON body into the target type: missing field `action`")?;
        if !action.is_object() {
            return Err("Failed to deserialize the JSON body into the target type: action must be an object".into());
        }
        let nonce = obj
            .get("nonce")
            .and_then(|v| v.as_u64())
            .ok_or("Failed to deserialize the JSON body into the target type: missing field `nonce`")?;
        let signature: Sig65 = serde_json::from_value(
            obj.get("signature").cloned().ok_or("Failed to deserialize the JSON body into the target type: missing field `signature`")?,
        )
        .map_err(|e| format!("Failed to deserialize the JSON body into the target type: signature: {e}"))?;
        let vault_address = match obj.get("vaultAddress") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(Address::from_hex(s).map_err(|e| format!("bad vaultAddress: {e}"))?),
            Some(_) => return Err("bad vaultAddress".into()),
        };
        let expires_after = match obj.get("expiresAfter") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_u64().ok_or("bad expiresAfter")?),
        };
        let mut action_bytes = Vec::with_capacity(256);
        to_msgpack(action, &mut action_bytes);
        Ok(UserTx { action_bytes, nonce, signature, vault_address, expires_after })
    }

    /// Minimal msgpack encoder for JSON values (map key order preserved).
    pub fn to_msgpack(v: &Value, out: &mut Vec<u8>) {
        match v {
            Value::Null => out.push(0xc0),
            Value::Bool(b) => out.push(if *b { 0xc3 } else { 0xc2 }),
            Value::Number(n) => {
                if let Some(u) = n.as_u64() {
                    write_uint(u, out)
                } else if let Some(i) = n.as_i64() {
                    write_int(i, out)
                } else {
                    let f = n.as_f64().unwrap_or(0.0);
                    out.push(0xcb);
                    out.extend_from_slice(&f.to_bits().to_be_bytes());
                }
            }
            Value::String(s) => {
                let b = s.as_bytes();
                let n = b.len();
                if n < 32 {
                    out.push(0xa0 | n as u8);
                } else if n < 256 {
                    out.push(0xd9);
                    out.push(n as u8);
                } else if n < 65536 {
                    out.push(0xda);
                    out.extend_from_slice(&(n as u16).to_be_bytes());
                } else {
                    out.push(0xdb);
                    out.extend_from_slice(&(n as u32).to_be_bytes());
                }
                out.extend_from_slice(b);
            }
            Value::Array(a) => {
                let n = a.len();
                if n < 16 {
                    out.push(0x90 | n as u8);
                } else if n < 65536 {
                    out.push(0xdc);
                    out.extend_from_slice(&(n as u16).to_be_bytes());
                } else {
                    out.push(0xdd);
                    out.extend_from_slice(&(n as u32).to_be_bytes());
                }
                for x in a {
                    to_msgpack(x, out);
                }
            }
            Value::Object(m) => {
                let n = m.len();
                if n < 16 {
                    out.push(0x80 | n as u8);
                } else if n < 65536 {
                    out.push(0xde);
                    out.extend_from_slice(&(n as u16).to_be_bytes());
                } else {
                    out.push(0xdf);
                    out.extend_from_slice(&(n as u32).to_be_bytes());
                }
                for (k, x) in m {
                    to_msgpack(&Value::String(k.clone()), out);
                    to_msgpack(x, out);
                }
            }
        }
    }

    fn write_uint(u: u64, out: &mut Vec<u8>) {
        if u < 128 {
            out.push(u as u8);
        } else if u < 256 {
            out.extend_from_slice(&[0xcc, u as u8]);
        } else if u < 65536 {
            out.push(0xcd);
            out.extend_from_slice(&(u as u16).to_be_bytes());
        } else if u < (1 << 32) {
            out.push(0xce);
            out.extend_from_slice(&(u as u32).to_be_bytes());
        } else {
            out.push(0xcf);
            out.extend_from_slice(&u.to_be_bytes());
        }
    }

    fn write_int(i: i64, out: &mut Vec<u8>) {
        if i >= 0 {
            return write_uint(i as u64, out);
        }
        if i >= -32 {
            out.push(i as i8 as u8);
        } else if i >= i8::MIN as i64 {
            out.extend_from_slice(&[0xd0, i as i8 as u8]);
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
}
