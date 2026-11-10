//! `hlnode testnet-gen`: deterministic genesis, keys and per-node configs.

use crate::config::NodeConfig;
use anyhow::{Context, Result};
use hlwire::Wallet;
use interfaces::*;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const DEFAULT_CHAIN_ID: u64 = 1337;
/// Default bridge contract: second contract deployed by Anvil dev account 0
/// on a fresh chain (MockUSDC first, Bridge second).
pub const DEFAULT_BRIDGE_CONTRACT: &str = "0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512";
pub const USDC_TOKEN_ID: &str = "6d1e7cde53ba9467b783cb7c530ce054";
pub const PURR_TOKEN_ID: &str = "c4bf3f870c0e9465323c0b6ed28096c2";

/// Perp USDC / spot USDC / PURR for the SDK example account.
pub const SDK_EXAMPLE_PERP_USD: i128 = 1_000_000;
pub const SDK_EXAMPLE_SPOT_USDC: i128 = 100_000;
pub const SDK_EXAMPLE_PURR: i128 = 100_000;
/// Perp USDC per funded test account.
pub const TEST_ACCOUNT_USD: i128 = 1_000_000;

#[derive(Clone, Debug)]
pub struct GenOpts {
    pub validators: usize,
    pub base_p2p: u16,
    pub base_api: u16,
    pub accounts: usize,
    pub fund_sdk_examples: bool,
    pub seed: u64,
    pub genesis_time_ms: u64,
    pub bridge_contract: Option<String>,
    pub host: String,
    /// Benchmark profile: no API rate limits, invariants off.
    pub bench: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountEntry {
    pub address: Address,
    pub secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidatorEntry {
    pub index: usize,
    pub address: Address,
    pub stake: u64,
    pub p2p_addr: String,
    pub api_addr: String,
    pub home: String,
}

/// Deterministic secp256k1 key from (seed, role, index).
pub fn derive_wallet(seed: u64, role: &str, index: u64) -> Wallet {
    let mut ctr = 0u32;
    loop {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"hlbft-testnet-key");
        buf.extend_from_slice(&seed.to_be_bytes());
        buf.extend_from_slice(role.as_bytes());
        buf.extend_from_slice(&index.to_be_bytes());
        buf.extend_from_slice(&ctr.to_be_bytes());
        let k = hlwire::keccak256(&buf);
        if let Ok(w) = Wallet::from_bytes(&k) {
            return w;
        }
        ctr += 1;
    }
}

/// The fixed (seed-independent) account the SDK examples use.
pub fn sdk_example_wallet() -> Wallet {
    derive_wallet(0, "sdk-example", 0)
}

fn px(units: u64, div: u64) -> Px {
    Px(units * PX_SCALE as u64 / div)
}

fn mk(asset: AssetId, name: &str, szd: u32, lev: u32, iso: bool, initial_px: Px) -> MarketSpec {
    MarketSpec { asset, name: name.into(), sz_decimals: szd, max_leverage: lev, only_isolated: iso, initial_px }
}

pub fn default_markets() -> Vec<MarketSpec> {
    vec![
        mk(0, "BTC", 5, 50, false, px(60_000, 1)),
        mk(1, "ETH", 4, 50, false, px(3_000, 1)),
        mk(2, "SOL", 2, 20, false, px(150, 1)),
        mk(3, "AVAX", 2, 20, false, px(30, 1)),
        mk(4, "DOGE", 0, 10, false, px(15, 100)),
        mk(5, "ARB", 1, 10, false, px(1, 1)),
    ]
}

fn token_id(h: &str) -> [u8; 16] {
    let v = hex::decode(h).expect("static hex");
    let mut o = [0u8; 16];
    o.copy_from_slice(&v);
    o
}

pub fn spot_tokens() -> Vec<SpotToken> {
    vec![
        SpotToken { index: 0, name: "USDC".into(), sz_decimals: 8, wei_decimals: 8, token_id: token_id(USDC_TOKEN_ID) },
        SpotToken { index: 1, name: "PURR".into(), sz_decimals: 0, wei_decimals: 5, token_id: token_id(PURR_TOKEN_ID) },
    ]
}

pub struct Generated {
    pub genesis: Genesis,
    pub validator_wallets: Vec<Wallet>,
    pub accounts: Vec<AccountEntry>,
    pub validators: Vec<ValidatorEntry>,
}

pub fn generate(o: &GenOpts) -> Result<Generated> {
    anyhow::ensure!(o.validators >= 1, "need at least one validator");
    let wallets: Vec<Wallet> = (0..o.validators).map(|i| derive_wallet(o.seed, "validator", i as u64)).collect();
    let mut vinfos = Vec::new();
    let mut ventries = Vec::new();
    for (i, w) in wallets.iter().enumerate() {
        let p2p = format!("{}:{}", o.host, o.base_p2p as usize + i);
        let api = format!("{}:{}", o.host, o.base_api as usize + i);
        vinfos.push(ValidatorInfo { address: w.address(), stake: 100, p2p_addr: p2p.clone() });
        ventries.push(ValidatorEntry {
            index: i,
            address: w.address(),
            stake: 100,
            p2p_addr: p2p,
            api_addr: api,
            home: format!("node{i}"),
        });
    }
    let mut accounts = Vec::new();
    let mut balances = Vec::new();
    let mut spot_balances = Vec::new();
    if o.fund_sdk_examples {
        let w = sdk_example_wallet();
        accounts.push(AccountEntry { address: w.address(), secret: w.secret_hex(), name: Some("sdk_example".into()) });
        balances.push((w.address(), Usd::from_raw(SDK_EXAMPLE_PERP_USD * USD_SCALE)));
        spot_balances.push((w.address(), 0u32, Usd::from_raw(SDK_EXAMPLE_SPOT_USDC * USD_SCALE)));
        spot_balances.push((w.address(), 1u32, Usd::from_raw(SDK_EXAMPLE_PURR * USD_SCALE)));
    }
    for i in 0..o.accounts {
        let w = derive_wallet(o.seed, "account", i as u64);
        accounts.push(AccountEntry { address: w.address(), secret: w.secret_hex(), name: None });
        balances.push((w.address(), Usd::from_raw(TEST_ACCOUNT_USD * USD_SCALE)));
    }
    let contract = Address::from_hex(o.bridge_contract.as_deref().unwrap_or(DEFAULT_BRIDGE_CONTRACT))
        .map_err(|e| anyhow::anyhow!("bad --bridge-contract: {e}"))?;
    let genesis = Genesis {
        chain_id: DEFAULT_CHAIN_ID,
        is_mainnet: false,
        genesis_time_ms: o.genesis_time_ms,
        validators: ValidatorSet { validators: vinfos },
        markets: default_markets(),
        balances,
        spot_tokens: spot_tokens(),
        spot_balances,
        params: ChainParams::default(),
        bridge: BridgeParams { eth_chain_id: 31337, contract, min_deposit: Usd::from_raw(5 * USD_SCALE) },
    };
    Ok(Generated { genesis, validator_wallets: wallets, accounts, validators: ventries })
}

pub fn write(dir: &Path, o: &GenOpts) -> Result<Generated> {
    let g = generate(o)?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(dir.join("genesis.json"), serde_json::to_string_pretty(&g.genesis)?)?;
    std::fs::write(dir.join("accounts.json"), serde_json::to_string_pretty(&g.accounts)?)?;
    std::fs::write(dir.join("validators.json"), serde_json::to_string_pretty(&g.validators)?)?;
    for (i, w) in g.validator_wallets.iter().enumerate() {
        let home = dir.join(format!("node{i}"));
        std::fs::create_dir_all(&home)?;
        std::fs::write(home.join("validator.key"), format!("{}\n", w.secret_hex()))?;
        let mut cfg = NodeConfig {
            genesis: Some("../genesis.json".into()),
            key: Some("validator.key".into()),
            p2p_listen: Some(g.validators[i].p2p_addr.clone()),
            api_listen: Some(g.validators[i].api_addr.clone()),
            oracle_feed: Some(format!("synthetic:{}", o.seed)),
            ..NodeConfig::default()
        };
        if o.bench {
            cfg.invariants = Some("off".into());
            cfg.mempool.ip_rate_per_s = 0;
            cfg.mempool.signer_rate_per_s = 0;
        }
        std::fs::write(home.join("config.toml"), toml::to_string_pretty(&cfg)?)?;
    }
    Ok(g)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn opts() -> GenOpts {
        GenOpts {
            validators: 4,
            base_p2p: 26600,
            base_api: 3001,
            accounts: 3,
            fund_sdk_examples: true,
            seed: 7,
            genesis_time_ms: 1_700_000_000_000,
            bridge_contract: None,
            host: "127.0.0.1".into(),
            bench: false,
        }
    }
    #[test]
    fn deterministic() {
        let a = generate(&opts()).unwrap();
        let b = generate(&opts()).unwrap();
        assert_eq!(a.genesis, b.genesis);
        assert_eq!(a.genesis.validators.validators.len(), 4);
        assert_eq!(a.accounts.len(), 4);
        let mut o = opts();
        o.seed = 8;
        let c = generate(&o).unwrap();
        assert_ne!(a.genesis.validators, c.genesis.validators);
        // sdk example account is seed independent
        assert_eq!(a.accounts[0].address, c.accounts[0].address);
        assert_eq!(hex::encode(a.genesis.spot_tokens[1].token_id), PURR_TOKEN_ID);
        assert_eq!(a.genesis.markets[0].name, "BTC");
        assert_eq!(a.genesis.markets[2].max_leverage, 20);
    }
}
