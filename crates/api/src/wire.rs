//! Bridge to `crates/hlwire`: `/exchange` JSON body -> [`UserTx`]
//! (action msgpack-encoded with key order preserved, as HL hashes it).

use interfaces::UserTx;
use serde_json::Value;

pub fn parse_exchange_request(body: &Value) -> Result<UserTx, String> {
    hlwire::parse_exchange_request(body)
}
