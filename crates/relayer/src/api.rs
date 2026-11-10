//! L1 API client: `POST /info {"type":"pendingWithdrawals"}`.
//!
//! Wire shape (see crates/relayer/README.md):
//! ```json
//! [{"id": 3, "user": "0x..", "destination": "0x..", "amount": "499.0",
//!   "created_at": 1700000000000,
//!   "signatures": [{"validator": "0x..", "r": "0x..", "s": "0x..", "v": 27}]}]
//! ```
//! For robustness the parser also accepts: the serde form of
//! `interfaces::PendingWithdrawal` (`amount` as raw 1e-6 integer,
//! `signatures` as `[[validator, {r,s,v}], ...]`), `createdAt`, a string `id`,
//! and the array wrapped as `{"pendingWithdrawals": [...]}`.

use anyhow::{anyhow, bail, Context, Result};
use interfaces::{Address, PendingWithdrawal, Sig65, Usd};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(untagged)]
enum WireSig {
    Obj { validator: Address, r: String, s: String, v: u8 },
    Pair(Address, Sig65),
}

#[derive(Deserialize)]
struct WireWithdrawal {
    id: Value,
    #[serde(default)]
    user: Option<Address>,
    destination: Address,
    amount: Value,
    #[serde(default, alias = "createdAt")]
    created_at: Option<Value>,
    #[serde(default)]
    signatures: Vec<WireSig>,
}

fn value_u64(v: &Value) -> Result<u64> {
    match v {
        Value::Number(n) => n.as_u64().ok_or_else(|| anyhow!("not a u64: {n}")),
        Value::String(s) => Ok(s.parse()?),
        _ => bail!("expected integer, got {v}"),
    }
}

fn sig_from_parts(r: &str, s: &str, v: u8) -> Result<Sig65> {
    let j = serde_json::json!({"r": r, "s": s, "v": v});
    Ok(serde_json::from_value(j)?)
}

fn convert(w: WireWithdrawal) -> Result<PendingWithdrawal> {
    let amount = match &w.amount {
        // decimal USD string, e.g. "499" or "499.5"
        Value::String(s) => Usd::parse(s).map_err(|e| anyhow!("bad amount {s:?}: {e:?}"))?,
        // raw 1e-6 integer (serde form of interfaces::Usd)
        Value::Number(n) => Usd::from_raw(n.as_i64().ok_or_else(|| anyhow!("bad amount {n}"))? as i128),
        other => bail!("bad amount {other}"),
    };
    let signatures = w
        .signatures
        .into_iter()
        .map(|s| match s {
            WireSig::Obj { validator, r, s, v } => Ok((validator, sig_from_parts(&r, &s, v)?)),
            WireSig::Pair(a, sig) => Ok((a, sig)),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(PendingWithdrawal {
        id: value_u64(&w.id)?,
        user: w.user.unwrap_or(Address::ZERO),
        destination: w.destination,
        amount,
        created_at: w.created_at.as_ref().map(value_u64).transpose()?.unwrap_or(0),
        signatures,
    })
}

/// Parse a `pendingWithdrawals` response body.
pub fn parse_pending_withdrawals(v: Value) -> Result<Vec<PendingWithdrawal>> {
    let arr = match v {
        Value::Array(a) => a,
        Value::Object(mut o) => match o.remove("pendingWithdrawals") {
            Some(Value::Array(a)) => a,
            _ => bail!("unexpected pendingWithdrawals response object"),
        },
        Value::Null => vec![],
        other => bail!("unexpected pendingWithdrawals response: {other}"),
    };
    arr.into_iter()
        .map(|item| convert(serde_json::from_value(item).context("bad pending withdrawal")?))
        .collect()
}

/// Wire JSON for one withdrawal (what the API should return; used by tests and docs).
pub fn to_wire(w: &PendingWithdrawal) -> Value {
    serde_json::json!({
        "id": w.id,
        "user": w.user,
        "destination": w.destination,
        "amount": w.amount.to_wire(),
        "created_at": w.created_at,
        "signatures": w.signatures.iter().map(|(a, s)| serde_json::json!({
            "validator": a, "r": format!("0x{}", hex::encode(s.r)), "s": format!("0x{}", hex::encode(s.s)), "v": s.v,
        })).collect::<Vec<_>>(),
    })
}

pub struct ApiClient {
    base: String,
    http: reqwest::Client,
}

impl ApiClient {
    /// `base` like `http://127.0.0.1:3001` (the `/info` path is appended).
    pub fn new(base: impl Into<String>) -> Self {
        let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build().expect("reqwest");
        Self { base: base.into().trim_end_matches('/').to_string(), http }
    }

    pub async fn pending_withdrawals(&self) -> Result<Vec<PendingWithdrawal>> {
        let url = format!("{}/info", self.base);
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({"type": "pendingWithdrawals"}))
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let body: Value = resp.json().await.with_context(|| format!("{url}: bad JSON"))?;
        if !status.is_success() {
            bail!("{url}: HTTP {status}: {body}");
        }
        parse_pending_withdrawals(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PendingWithdrawal {
        PendingWithdrawal {
            id: 3,
            user: Address([1; 20]),
            destination: Address([2; 20]),
            amount: Usd::from_raw(499_000_000),
            created_at: 1_700_000_000_000,
            signatures: vec![(Address([3; 20]), Sig65 { r: [4; 32], s: [5; 32], v: 28 })],
        }
    }

    #[test]
    fn wire_roundtrip() {
        let w = sample();
        let v = serde_json::json!([to_wire(&w)]);
        assert_eq!(v[0]["amount"], "499");
        assert_eq!(parse_pending_withdrawals(v).unwrap(), vec![w]);
    }

    #[test]
    fn accepts_interfaces_serde_form() {
        let w = sample();
        let v = serde_json::to_value(vec![w.clone()]).unwrap();
        assert_eq!(parse_pending_withdrawals(v).unwrap(), vec![w]);
    }

    #[test]
    fn accepts_variants() {
        let v = serde_json::json!({"pendingWithdrawals": [{
            "id": "9", "destination": "0x0202020202020202020202020202020202020202",
            "amount": "0.5", "createdAt": 5, "signatures": []
        }]});
        let ws = parse_pending_withdrawals(v).unwrap();
        assert_eq!(ws[0].id, 9);
        assert_eq!(ws[0].amount, Usd::from_raw(500_000));
        assert_eq!(ws[0].created_at, 5);
        assert!(parse_pending_withdrawals(serde_json::json!(null)).unwrap().is_empty());
        assert!(parse_pending_withdrawals(serde_json::json!([{"id": 1}])).is_err());
        assert!(parse_pending_withdrawals(serde_json::json!("x")).is_err());
    }
}
