//! Deterministic genesis + key material for simulations and localnets.

use crate::agents::AgentKind;
use hlwire::Wallet;
use interfaces::*;
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Fixed genesis time (2026-01-01T00:00:00Z) so logs never depend on the clock.
pub const DEFAULT_GENESIS_TIME_MS: u64 = 1_767_225_600_000;
pub const DEFAULT_CHAIN_ID: u64 = 1337;
/// USDC spot token id (HL mainnet/testnet USDC tokenId).
pub const USDC_TOKEN_ID: [u8; 16] = hex_lit("6d1e7cde53ba9467b783cb7c530ce054");
/// PURR spot token id (as used by the SDK examples).
pub const PURR_TOKEN_ID: [u8; 16] = hex_lit("c4bf3f870c0e9465323c0b6ed28096c2");

const fn hex_lit(s: &str) -> [u8; 16] {
    let b = s.as_bytes();
    let mut out = [0u8; 16];
    let mut i = 0;
    while i < 16 {
        out[i] = (nib(b[2 * i]) << 4) | nib(b[2 * i + 1]);
        i += 1;
    }
    out
}
const fn nib(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => panic!("bad hex"),
    }
}

/// `"PURR:0x<tokenId>"` (spotSend token string).
pub fn purr_token_str() -> String {
    format!("PURR:0x{}", hex::encode(PURR_TOKEN_ID))
}

fn px(units: u64, decimals_div: u64) -> Px {
    Px(units * PX_SCALE as u64 / decimals_div)
}

fn mk(asset: AssetId, name: &str, sz_decimals: u32, max_leverage: u32, only_isolated: bool, initial_px: Px) -> MarketSpec {
    MarketSpec { asset, name: name.into(), sz_decimals, max_leverage, only_isolated, initial_px }
}

/// BTC, ETH, SOL, AVAX, DOGE, ARB and kPEPE (isolated-only), HL-like
/// size decimals / max leverage.
pub fn default_markets() -> Vec<MarketSpec> {
    vec![
        mk(0, "BTC", 5, 50, false, px(60_000, 1)),
        mk(1, "ETH", 4, 50, false, px(3_000, 1)),
        mk(2, "SOL", 2, 20, false, px(150, 1)),
        mk(3, "AVAX", 2, 20, false, px(30, 1)),
        mk(4, "DOGE", 0, 10, false, px(15, 100)),
        mk(5, "ARB", 1, 10, false, px(1, 1)),
        mk(6, "kPEPE", 0, 10, true, px(12, 1000)),
    ]
}

/// Market subset by names (`"BTC,ETH,SOL"`); assets are renumbered 0..n.
pub fn markets_by_name(names: &str) -> anyhow::Result<Vec<MarketSpec>> {
    let all = default_markets();
    let mut out = Vec::new();
    for n in names.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let mut m = all
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case(n))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown market {n}"))?;
        m.asset = out.len() as AssetId;
        out.push(m);
    }
    anyhow::ensure!(!out.is_empty(), "no markets");
    Ok(out)
}

/// Keys behind a simulated genesis.
#[derive(Clone, Debug)]
pub struct SimKeys {
    pub seed: u64,
    pub validators: Vec<Wallet>,
    pub accounts: Vec<Wallet>,
    /// Approved-agent ("API wallet") keys, one per account (only some
    /// accounts approve them during a simulation).
    pub agents: Vec<Wallet>,
    pub kinds: Vec<AgentKind>,
}

fn seeded(seed: u64, domain: &str) -> ChaCha20Rng {
    let h = blake3::hash(&[&seed.to_le_bytes()[..], domain.as_bytes()].concat());
    ChaCha20Rng::from_seed(*h.as_bytes())
}

impl SimKeys {
    pub fn derive(seed: u64, n_validators: usize, n_accounts: usize) -> SimKeys {
        let mut vr = seeded(seed, "hlbft-sim/validators");
        let validators = (0..n_validators).map(|_| Wallet::random(&mut vr)).collect();
        let mut ar = seeded(seed, "hlbft-sim/accounts");
        let accounts = (0..n_accounts).map(|_| Wallet::random(&mut ar)).collect();
        let mut gr = seeded(seed, "hlbft-sim/agents");
        let agents = (0..n_accounts).map(|_| Wallet::random(&mut gr)).collect();
        SimKeys { seed, validators, accounts, agents, kinds: crate::agents::assign_kinds(n_accounts) }
    }

    /// Wallet unknown to the chain (for "does not exist" error paths).
    pub fn stranger(&self, i: u64) -> Wallet {
        let mut r = seeded(self.seed ^ i.rotate_left(17), "hlbft-sim/stranger");
        Wallet::random(&mut r)
    }

    pub fn to_file(&self) -> KeysFile {
        KeysFile {
            seed: self.seed,
            validators: self
                .validators
                .iter()
                .map(|w| KeyEntry { address: w.address(), secret: w.secret_hex(), kind: None })
                .collect(),
            accounts: self
                .accounts
                .iter()
                .zip(&self.kinds)
                .map(|(w, k)| KeyEntry { address: w.address(), secret: w.secret_hex(), kind: Some(format!("{k:?}")) })
                .collect(),
            agents: self
                .agents
                .iter()
                .map(|w| KeyEntry { address: w.address(), secret: w.secret_hex(), kind: None })
                .collect(),
        }
    }

    pub fn from_file(f: &KeysFile) -> anyhow::Result<SimKeys> {
        let parse = |v: &[KeyEntry]| -> anyhow::Result<Vec<Wallet>> {
            v.iter().map(|e| Wallet::from_secret_hex(&e.secret).map_err(|e| anyhow::anyhow!(e))).collect()
        };
        let accounts = parse(&f.accounts)?;
        let mut agents = parse(&f.agents)?;
        if agents.len() < accounts.len() {
            let d = SimKeys::derive(f.seed, 0, accounts.len());
            agents = d.agents;
        }
        let kinds = crate::agents::assign_kinds(accounts.len());
        Ok(SimKeys { seed: f.seed, validators: parse(&f.validators)?, accounts, agents, kinds })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeyEntry {
    pub address: Address,
    pub secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// JSON keys file (`keys.json`) written next to `genesis.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeysFile {
    pub seed: u64,
    pub validators: Vec<KeyEntry>,
    pub accounts: Vec<KeyEntry>,
    #[serde(default)]
    pub agents: Vec<KeyEntry>,
}

/// Options beyond the four positional parameters of [`genesis`].
#[derive(Clone, Debug)]
pub struct GenesisOpts {
    pub chain_id: u64,
    pub genesis_time_ms: u64,
    /// p2p address of validator i = `{p2p_host}:{p2p_base_port + i}`.
    pub p2p_host: String,
    pub p2p_base_port: u16,
    pub params: ChainParams,
    pub bridge: BridgeParams,
}

impl Default for GenesisOpts {
    fn default() -> Self {
        GenesisOpts {
            chain_id: DEFAULT_CHAIN_ID,
            genesis_time_ms: DEFAULT_GENESIS_TIME_MS,
            p2p_host: "127.0.0.1".into(),
            p2p_base_port: 9000,
            params: ChainParams::default(),
            bridge: BridgeParams {
                eth_chain_id: 31337,
                contract: Address::ZERO,
                min_deposit: Usd::from_raw(5 * USD_SCALE),
            },
        }
    }
}

/// Genesis perp collateral (USD, whole dollars) per agent kind.
pub fn genesis_balance_usd(kind: AgentKind) -> i64 {
    match kind {
        AgentKind::MarketMaker => 5_000_000,
        AgentKind::Taker => 200_000,
        AgentKind::TrendFollower => 200_000,
        AgentKind::TpSl => 100_000,
        AgentKind::Leverage => 100_000,
        AgentKind::Transfer => 50_000,
        AgentKind::WashTrader => 500_000,
        AgentKind::LiqHunter => 2_000_000,
        AgentKind::Degen => 2_000,
    }
}

/// Deterministic genesis with `n_validators` (equal stake) and `n_accounts`
/// funded accounts. Spot tokens: USDC (0) and PURR (1).
pub fn genesis(seed: u64, n_validators: usize, n_accounts: usize, markets: &[MarketSpec]) -> (Genesis, SimKeys) {
    genesis_with(seed, n_validators, n_accounts, markets, &GenesisOpts::default())
}

pub fn genesis_with(
    seed: u64,
    n_validators: usize,
    n_accounts: usize,
    markets: &[MarketSpec],
    o: &GenesisOpts,
) -> (Genesis, SimKeys) {
    assert!(n_validators >= 1, "need at least one validator");
    let keys = SimKeys::derive(seed, n_validators, n_accounts);
    let validators = ValidatorSet {
        validators: keys
            .validators
            .iter()
            .enumerate()
            .map(|(i, w)| ValidatorInfo {
                address: w.address(),
                stake: 100,
                p2p_addr: format!("{}:{}", o.p2p_host, o.p2p_base_port as usize + i),
            })
            .collect(),
    };
    let mut markets = markets.to_vec();
    for (i, m) in markets.iter_mut().enumerate() {
        m.asset = i as AssetId;
    }
    let balances = keys
        .accounts
        .iter()
        .zip(&keys.kinds)
        .map(|(w, k)| (w.address(), Usd::from_raw(genesis_balance_usd(*k) as i128 * USD_SCALE)))
        .collect();
    let mut spot_balances = Vec::new();
    for w in &keys.accounts {
        spot_balances.push((w.address(), 0u32, Usd::from_raw(1_000 * USD_SCALE)));
        spot_balances.push((w.address(), 1u32, Usd::from_raw(10_000 * USD_SCALE)));
    }
    let g = Genesis {
        chain_id: o.chain_id,
        is_mainnet: false,
        genesis_time_ms: o.genesis_time_ms,
        validators,
        markets,
        balances,
        spot_tokens: vec![
            SpotToken { index: 0, name: "USDC".into(), sz_decimals: 8, wei_decimals: 8, token_id: USDC_TOKEN_ID },
            SpotToken { index: 1, name: "PURR".into(), sz_decimals: 0, wei_decimals: 5, token_id: PURR_TOKEN_ID },
        ],
        spot_balances,
        params: o.params.clone(),
        bridge: o.bridge.clone(),
    };
    (g, keys)
}

/// blake3 of the borsh-encoded genesis.
pub fn genesis_hash(g: &Genesis) -> Hash32 {
    hash_borsh(g)
}

/// Write `genesis.json` and `keys.json` into `dir`.
pub fn write_genesis_files(dir: &Path, g: &Genesis, keys: &SimKeys) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("genesis.json"), serde_json::to_vec_pretty(g)?)?;
    std::fs::write(dir.join("keys.json"), serde_json::to_vec_pretty(&keys.to_file())?)?;
    Ok(())
}

pub fn read_keys_file(path: &Path) -> anyhow::Result<SimKeys> {
    let f: KeysFile = serde_json::from_slice(&std::fs::read(path)?)?;
    SimKeys::from_file(&f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genesis_is_deterministic() {
        let (g1, k1) = genesis(7, 4, 50, &default_markets());
        let (g2, k2) = genesis(7, 4, 50, &default_markets());
        assert_eq!(g1, g2);
        assert_eq!(genesis_hash(&g1), genesis_hash(&g2));
        assert_eq!(k1.accounts, k2.accounts);
        let (g3, _) = genesis(8, 4, 50, &default_markets());
        assert_ne!(genesis_hash(&g1), genesis_hash(&g3));
        assert_eq!(g1.validators.validators.len(), 4);
        assert_eq!(g1.balances.len(), 50);
        for m in &g1.markets {
            assert!(m.is_valid_px(m.initial_px), "{}", m.name);
        }
    }

    #[test]
    fn keys_file_roundtrip() {
        let (_, k) = genesis(3, 4, 10, &default_markets());
        let f = k.to_file();
        let j = serde_json::to_string(&f).unwrap();
        let k2 = SimKeys::from_file(&serde_json::from_str(&j).unwrap()).unwrap();
        assert_eq!(k.accounts, k2.accounts);
        assert_eq!(k.validators, k2.validators);
        assert_eq!(k.agents, k2.agents);
    }

    #[test]
    fn genesis_json_roundtrip() {
        let (g, _) = genesis(3, 4, 10, &default_markets());
        let j = serde_json::to_string(&g).unwrap();
        let g2: Genesis = serde_json::from_str(&j).unwrap();
        assert_eq!(g, g2);
    }
}
