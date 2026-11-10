//! Minimal Ethereum JSON-RPC client over reqwest (HTTP).

use anyhow::{anyhow, bail, Context, Result};
use interfaces::{Address, Hash32};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Log {
    pub address: Address,
    pub topics: Vec<Hash32>,
    pub data: Vec<u8>,
    pub block_number: u64,
    pub block_hash: Hash32,
    pub tx_hash: Hash32,
    pub log_index: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub tx_hash: Hash32,
    pub status: bool,
    pub block_number: u64,
    pub contract_address: Option<Address>,
    pub logs: Vec<Log>,
}

pub struct EthRpc {
    url: String,
    http: reqwest::Client,
    next_id: AtomicU64,
}

pub fn parse_hex_u64(v: &Value) -> Result<u64> {
    let s = v.as_str().ok_or_else(|| anyhow!("expected hex string, got {v}"))?;
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.is_empty() {
        return Ok(0);
    }
    Ok(u64::from_str_radix(s, 16)?)
}

pub fn parse_hex_bytes(v: &Value) -> Result<Vec<u8>> {
    let s = v.as_str().ok_or_else(|| anyhow!("expected hex string, got {v}"))?;
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.len() % 2 == 1 {
        return Ok(hex::decode(format!("0{s}"))?);
    }
    Ok(hex::decode(s)?)
}

fn parse_hash(v: &Value) -> Result<Hash32> {
    Hash32::from_hex(v.as_str().ok_or_else(|| anyhow!("expected hash, got {v}"))?).map_err(|e| anyhow!("{e:?}"))
}

fn parse_address(v: &Value) -> Result<Address> {
    Address::from_hex(v.as_str().ok_or_else(|| anyhow!("expected address, got {v}"))?).map_err(|e| anyhow!("{e:?}"))
}

pub fn hex_u64(v: u64) -> String {
    format!("0x{v:x}")
}

pub fn hex_data(b: &[u8]) -> String {
    format!("0x{}", hex::encode(b))
}

fn parse_log(v: &Value) -> Result<Log> {
    Ok(Log {
        address: parse_address(&v["address"])?,
        topics: v["topics"]
            .as_array()
            .ok_or_else(|| anyhow!("log without topics"))?
            .iter()
            .map(parse_hash)
            .collect::<Result<_>>()?,
        data: parse_hex_bytes(&v["data"])?,
        block_number: parse_hex_u64(&v["blockNumber"])?,
        block_hash: parse_hash(&v["blockHash"])?,
        tx_hash: parse_hash(&v["transactionHash"])?,
        log_index: parse_hex_u64(&v["logIndex"])?,
    })
}

impl EthRpc {
    pub fn new(url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client");
        Self { url: url.into(), http, next_id: AtomicU64::new(1) }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Raw JSON-RPC call; returns `result` or an error carrying the RPC error.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let resp: Value = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("{method}: POST {}", self.url))?
            .json()
            .await
            .with_context(|| format!("{method}: bad JSON response"))?;
        if let Some(err) = resp.get("error") {
            bail!("{method}: rpc error {err}");
        }
        resp.get("result").cloned().ok_or_else(|| anyhow!("{method}: response without result"))
    }

    pub async fn chain_id(&self) -> Result<u64> {
        parse_hex_u64(&self.request("eth_chainId", json!([])).await?)
    }

    pub async fn block_number(&self) -> Result<u64> {
        parse_hex_u64(&self.request("eth_blockNumber", json!([])).await?)
    }

    /// Hash of block `n` (None if it does not exist).
    pub async fn block_hash(&self, n: u64) -> Result<Option<Hash32>> {
        let v = self.request("eth_getBlockByNumber", json!([hex_u64(n), false])).await?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(parse_hash(&v["hash"])?))
    }

    /// `eth_getLogs` for one contract; `topic0s` is OR-ed.
    pub async fn get_logs(&self, address: &Address, topic0s: &[Hash32], from: u64, to: u64) -> Result<Vec<Log>> {
        let topics: Vec<String> = topic0s.iter().map(|t| t.to_hex()).collect();
        let filter = json!({
            "address": address.to_hex(),
            "fromBlock": hex_u64(from),
            "toBlock": hex_u64(to),
            "topics": [topics],
        });
        let v = self.request("eth_getLogs", json!([filter])).await?;
        let mut logs: Vec<Log> =
            v.as_array().ok_or_else(|| anyhow!("eth_getLogs: not an array"))?.iter().map(parse_log).collect::<Result<_>>()?;
        logs.sort_by_key(|l| (l.block_number, l.log_index));
        Ok(logs)
    }

    /// `eth_call` at `block` ("latest" if None).
    pub async fn call(&self, to: &Address, data: &[u8], block: Option<u64>) -> Result<Vec<u8>> {
        let tag = block.map(hex_u64).unwrap_or_else(|| "latest".into());
        let v = self.request("eth_call", json!([{"to": to.to_hex(), "data": hex_data(data)}, tag])).await?;
        parse_hex_bytes(&v)
    }

    pub async fn get_transaction_count(&self, a: &Address) -> Result<u64> {
        parse_hex_u64(&self.request("eth_getTransactionCount", json!([a.to_hex(), "pending"])).await?)
    }

    pub async fn gas_price(&self) -> Result<u128> {
        let v = self.request("eth_gasPrice", json!([])).await?;
        Ok(parse_hex_u64(&v)? as u128)
    }

    pub async fn estimate_gas(&self, from: &Address, to: Option<&Address>, data: &[u8]) -> Result<u64> {
        let mut tx = json!({"from": from.to_hex(), "data": hex_data(data)});
        if let Some(to) = to {
            tx["to"] = json!(to.to_hex());
        }
        parse_hex_u64(&self.request("eth_estimateGas", json!([tx])).await?)
    }

    pub async fn send_raw_transaction(&self, raw: &[u8]) -> Result<Hash32> {
        parse_hash(&self.request("eth_sendRawTransaction", json!([hex_data(raw)])).await?)
    }

    /// `eth_sendTransaction` from an unlocked account (Anvil dev accounts).
    pub async fn send_transaction(&self, from: &Address, to: &Address, data: &[u8]) -> Result<Hash32> {
        parse_hash(
            &self
                .request("eth_sendTransaction", json!([{"from": from.to_hex(), "to": to.to_hex(), "data": hex_data(data)}]))
                .await?,
        )
    }

    pub async fn get_transaction_receipt(&self, h: &Hash32) -> Result<Option<Receipt>> {
        let v = self.request("eth_getTransactionReceipt", json!([h.to_hex()])).await?;
        if v.is_null() {
            return Ok(None);
        }
        let contract_address = match &v["contractAddress"] {
            Value::Null => None,
            a => Some(parse_address(a)?),
        };
        Ok(Some(Receipt {
            tx_hash: parse_hash(&v["transactionHash"])?,
            status: parse_hex_u64(&v["status"])? == 1,
            block_number: parse_hex_u64(&v["blockNumber"])?,
            contract_address,
            logs: v["logs"].as_array().map(|ls| ls.iter().map(parse_log).collect::<Result<Vec<_>>>()).transpose()?.unwrap_or_default(),
        }))
    }

    /// Poll for a receipt until `timeout`.
    pub async fn wait_receipt(&self, h: &Hash32, timeout: Duration) -> Result<Receipt> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(r) = self.get_transaction_receipt(h).await? {
                return Ok(r);
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("timed out waiting for receipt of {h}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Anvil/hardhat: mine `n` blocks.
    pub async fn anvil_mine(&self, n: u64) -> Result<()> {
        self.request("anvil_mine", json!([hex_u64(n)])).await.map(|_| ())
    }
}
