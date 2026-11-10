//! `hlrelayer`: withdrawal relayer + bridge helper commands for e2e scripts.
//!
//! ```text
//! hlrelayer relay   --api http://127.0.0.1:3001 [--rpc ..] [--deployment bridge/deployments/local.json]
//! hlrelayer deposit --dev-account 1 --amount 1000          # approve + deposit, prints JSON
//! hlrelayer mint    --to 0x.. --amount 1000                # MockUSDC.mint
//! hlrelayer balance --address 0x..                         # USDC balance JSON
//! hlrelayer info                                           # bridge state JSON
//! ```

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use interfaces::{Address, Usd};
use relayer::api::ApiClient;
use relayer::contract::{
    approve_calldata, decode_deposit, deposit_calldata, deposit_topic, erc20_balance, mint_calldata, BridgeView,
};
use relayer::eip712::parse_secret_key;
use relayer::relay::{RelayOutcome, Relayer};
use relayer::rpc::EthRpc;
use relayer::txsign::TxSender;
use secp256k1::SecretKey;
use serde_json::json;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "hlrelayer", about = "hlbft bridge relayer and helpers")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Clone)]
struct Chain {
    /// EVM JSON-RPC URL.
    #[arg(long, env = "HL_BRIDGE_RPC", default_value = "http://127.0.0.1:8545")]
    rpc: String,
    /// Bridge contract address (overrides --deployment).
    #[arg(long, env = "HL_BRIDGE_ADDRESS", value_parser = parse_addr)]
    bridge: Option<Address>,
    /// Deployment JSON written by bridge/deploy.sh.
    #[arg(long, env = "HL_BRIDGE_DEPLOYMENT", default_value = "bridge/deployments/local.json")]
    deployment: String,
}

#[derive(Args, Clone)]
struct Key {
    /// Hex private key of the sending account.
    #[arg(long, env = "HL_RELAYER_KEY")]
    key: Option<String>,
    /// Use Anvil dev account N (0..9) instead of --key.
    #[arg(long)]
    dev_account: Option<usize>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Poll the L1 API for pending withdrawals and submit them once signed by a quorum.
    Relay {
        #[command(flatten)]
        chain: Chain,
        #[command(flatten)]
        key: Key,
        /// L1 API base URL (POST {api}/info).
        #[arg(long, env = "HL_API_URL", default_value = "http://127.0.0.1:3001")]
        api: String,
        #[arg(long, default_value_t = 1000)]
        poll_ms: u64,
        /// Run a single iteration and exit.
        #[arg(long)]
        once: bool,
    },
    /// Approve + deposit USDC into the bridge from a dev account.
    Deposit {
        #[command(flatten)]
        chain: Chain,
        #[command(flatten)]
        key: Key,
        /// Decimal USDC amount, e.g. "1000" or "12.5".
        #[arg(long)]
        amount: String,
    },
    /// Mint MockUSDC to an address.
    Mint {
        #[command(flatten)]
        chain: Chain,
        #[command(flatten)]
        key: Key,
        #[arg(long, value_parser = parse_addr)]
        to: Address,
        #[arg(long)]
        amount: String,
    },
    /// Print an address's USDC balance as JSON.
    Balance {
        #[command(flatten)]
        chain: Chain,
        #[arg(long, value_parser = parse_addr)]
        address: Address,
    },
    /// Print bridge state as JSON.
    Info {
        #[command(flatten)]
        chain: Chain,
    },
}

impl Chain {
    fn bridge(&self) -> Result<Address> {
        if let Some(b) = self.bridge {
            return Ok(b);
        }
        let s = std::fs::read_to_string(&self.deployment)
            .with_context(|| format!("no --bridge given and cannot read {}", self.deployment))?;
        let v: serde_json::Value = serde_json::from_str(&s)?;
        Address::from_hex(v["bridge"].as_str().ok_or_else(|| anyhow!("{}: no \"bridge\"", self.deployment))?)
            .map_err(|e| anyhow!("bad bridge address: {e:?}"))
    }
}

impl Key {
    /// Default: Anvil dev account `default_dev`.
    fn secret(&self, default_dev: usize) -> Result<SecretKey> {
        if let Some(k) = &self.key {
            return parse_secret_key(k);
        }
        let i = self.dev_account.unwrap_or(default_dev);
        let k = relayer::ANVIL_DEV_KEYS.get(i).ok_or_else(|| anyhow!("dev account must be 0..9"))?;
        parse_secret_key(k)
    }
}

fn parse_addr(s: &str) -> Result<Address, String> {
    Address::from_hex(s).map_err(|e| format!("bad address {s:?}: {e:?}"))
}

fn parse_usdc(s: &str) -> Result<u128> {
    let u = Usd::parse(s).map_err(|e| anyhow!("bad amount {s:?}: {e:?}"))?;
    if u.raw() <= 0 {
        bail!("amount must be positive");
    }
    Ok(u.raw() as u128)
}

fn fmt_usdc(raw: u128) -> String {
    match i64::try_from(raw) {
        Ok(v) => Usd::from_raw(v as i128).to_wire(),
        Err(_) => format!("{raw}e-6"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Relay { chain, key, api, poll_ms, once } => {
            let bridge = chain.bridge()?;
            // Dev account 9 by default so the relayer does not race the
            // deployer (0) or test users for nonces.
            let mut relayer = Relayer::new(&chain.rpc, bridge, key.secret(9)?);
            let api = ApiClient::new(api);
            tracing::info!(%bridge, rpc = %chain.rpc, "relayer started");
            loop {
                match api.pending_withdrawals().await {
                    Ok(pending) => match relayer.process(&pending).await {
                        Ok(results) => {
                            for (id, r) in results {
                                if let RelayOutcome::Submitted { tx_hash } = r {
                                    println!("{}", json!({"event": "withdrawSubmitted", "id": id, "txHash": tx_hash}));
                                }
                            }
                        }
                        Err(e) => tracing::warn!("relay iteration failed: {e:#}"),
                    },
                    Err(e) => tracing::warn!("fetching pending withdrawals failed: {e:#}"),
                }
                if once {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(poll_ms)).await;
            }
        }
        Cmd::Deposit { chain, key, amount } => {
            let rpc = EthRpc::new(&chain.rpc);
            let bridge = chain.bridge()?;
            let amount = parse_usdc(&amount)?;
            let sender = TxSender::new(&rpc, key.secret(1)?).await?;
            let token = BridgeView::new(&rpc, bridge).token().await?;
            let approve = sender.send_and_wait(Some(&token), approve_calldata(&bridge, amount)).await?;
            let dep = sender.send_and_wait(Some(&bridge), deposit_calldata(amount)).await?;
            let ev = dep
                .logs
                .iter()
                .find(|l| l.address == bridge && l.topics.first() == Some(&deposit_topic()))
                .map(decode_deposit)
                .transpose()?
                .ok_or_else(|| anyhow!("no Deposit event in receipt"))?;
            println!(
                "{}",
                json!({
                    "depositId": ev.id, "user": ev.user, "amount": fmt_usdc(ev.amount), "amountRaw": ev.amount.to_string(),
                    "approveTx": approve.tx_hash, "depositTx": dep.tx_hash, "block": dep.block_number,
                })
            );
        }
        Cmd::Mint { chain, key, to, amount } => {
            let rpc = EthRpc::new(&chain.rpc);
            let token = BridgeView::new(&rpc, chain.bridge()?).token().await?;
            let sender = TxSender::new(&rpc, key.secret(0)?).await?;
            let r = sender.send_and_wait(Some(&token), mint_calldata(&to, parse_usdc(&amount)?)).await?;
            println!("{}", json!({"to": to, "amount": amount, "txHash": r.tx_hash}));
        }
        Cmd::Balance { chain, address } => {
            let rpc = EthRpc::new(&chain.rpc);
            let token = BridgeView::new(&rpc, chain.bridge()?).token().await?;
            let raw = erc20_balance(&rpc, &token, &address).await?;
            println!("{}", json!({"address": address, "usdc": fmt_usdc(raw), "raw": raw.to_string()}));
        }
        Cmd::Info { chain } => {
            let rpc = EthRpc::new(&chain.rpc);
            let bridge = chain.bridge()?;
            let v = BridgeView::new(&rpc, bridge);
            let (vals, powers) = v.validators().await?;
            let token = v.token().await?;
            println!(
                "{}",
                json!({
                    "bridge": bridge, "chainId": rpc.chain_id().await?, "usdc": token,
                    "validators": vals, "powers": powers.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                    "epoch": v.epoch().await?, "quorumPower": v.quorum_power().await?.to_string(),
                    "nextDepositId": v.next_deposit_id().await?, "minDeposit": fmt_usdc(v.min_deposit().await?),
                    "bridgeBalance": fmt_usdc(erc20_balance(&rpc, &token, &bridge).await?),
                })
            );
        }
    }
    Ok(())
}
