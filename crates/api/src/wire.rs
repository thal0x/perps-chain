//! Bridge to `crates/hlwire`: `/exchange` body -> [`UserTx`] (action
//! msgpack-encoded with key order preserved, as HL hashes it).

use interfaces::UserTx;
use serde_json::Value;

/// HL's text for bodies that are not valid JSON.
pub const DESERIALIZE_ERR: &str = "Failed to deserialize the JSON body into the target type";

/// Parsed JSON body (WebSocket `post` action payloads).
pub fn parse_exchange_request(body: &Value) -> Result<UserTx, String> {
    hlwire::parse_exchange_request(body)
}

/// Raw `/exchange` body bytes (hlwire's allocation-light fast path, with
/// its own authoritative fallback). Invalid JSON maps to HL's text.
pub fn parse_exchange_bytes(bytes: &[u8]) -> Result<UserTx, String> {
    hlwire::parse_exchange_request_bytes(bytes).map_err(|e| if e.starts_with("invalid JSON") { DESERIALIZE_ERR.to_string() } else { e })
}
