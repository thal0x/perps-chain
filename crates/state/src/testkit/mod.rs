//! Test fakes and helpers. Public so integration tests (and other crates'
//! tests) can drive a `Chain` without the real engines or real signatures.

pub mod book;
pub mod clearinghouse;
pub mod oracle;
pub mod verifier;

pub use book::{SimpleBook, SimpleTriggers};
pub use clearinghouse::SimpleClearinghouse;
pub use oracle::SimpleOracle;
pub use verifier::{fake_sig, user_tx, validator_tx, FakeVerifier};

use crate::engines::Engines;
use interfaces::*;

/// Engines bundle built from the fakes in this module.
pub struct FakeEngines;

impl Engines for FakeEngines {
    type Ch = SimpleClearinghouse;
    type Book = SimpleBook;
    type Trig = SimpleTriggers;
    type Oracle = SimpleOracle;
    fn new_clearinghouse(g: &Genesis) -> Self::Ch {
        SimpleClearinghouse::new(g)
    }
    fn decode_clearinghouse(_g: &Genesis, bytes: &[u8]) -> Result<Self::Ch, String> {
        SimpleClearinghouse::decode(bytes)
    }
    fn new_book(spec: &MarketSpec) -> Self::Book {
        SimpleBook::new(spec.asset)
    }
    fn decode_book(_spec: &MarketSpec, bytes: &[u8]) -> Result<Self::Book, String> {
        SimpleBook::decode(bytes)
    }
    fn new_triggers(_spec: &MarketSpec) -> Self::Trig {
        SimpleTriggers::default()
    }
    fn decode_triggers(_spec: &MarketSpec, bytes: &[u8]) -> Result<Self::Trig, String> {
        SimpleTriggers::decode(bytes)
    }
    fn new_oracle(g: &Genesis) -> Self::Oracle {
        SimpleOracle::new(g.params.oracle_freshness_ms)
    }
}

/// Deterministic test address `0x00..00<n>`.
pub fn addr(n: u64) -> Address {
    let mut a = [0u8; 20];
    a[12..].copy_from_slice(&n.to_be_bytes());
    Address(a)
}

pub fn usd(s: &str) -> Usd {
    Usd::parse(s).expect("usd")
}
pub fn px(s: &str) -> Px {
    Px::parse(s).expect("px")
}
pub fn sz(s: &str) -> Sz {
    Sz::parse(s).expect("sz")
}

pub const GENESIS_TIME: u64 = 1_750_000_000_000;

/// Genesis with BTC (sz 5 dec, 50x) and ETH (sz 4 dec, 25x), `n_validators`
/// equal-stake validators `addr(1000+i)`, and `balances`.
pub fn test_genesis(n_validators: usize, balances: &[(Address, Usd)]) -> Genesis {
    Genesis {
        chain_id: 1337,
        is_mainnet: false,
        genesis_time_ms: GENESIS_TIME,
        validators: ValidatorSet {
            validators: (0..n_validators)
                .map(|i| ValidatorInfo { address: addr(1000 + i as u64), stake: 100, p2p_addr: String::new() })
                .collect(),
        },
        markets: vec![
            MarketSpec {
                asset: 0,
                name: "BTC".into(),
                sz_decimals: 5,
                max_leverage: 50,
                only_isolated: false,
                initial_px: px("60000"),
            },
            MarketSpec {
                asset: 1,
                name: "ETH".into(),
                sz_decimals: 4,
                max_leverage: 25,
                only_isolated: false,
                initial_px: px("3000"),
            },
        ],
        balances: balances.to_vec(),
        spot_tokens: vec![
            SpotToken { index: 0, name: "USDC".into(), sz_decimals: 8, wei_decimals: 8, token_id: [0u8; 16] },
            SpotToken { index: 1, name: "PURR".into(), sz_decimals: 0, wei_decimals: 5, token_id: [0x11; 16] },
        ],
        spot_balances: vec![],
        params: ChainParams::default(),
        bridge: BridgeParams { eth_chain_id: 31337, contract: Address([0xcc; 20]), min_deposit: Usd::from_raw(1) },
    }
}

/// Convenience constructors for typed orders.
pub fn limit(asset: AssetId, is_buy: bool, p: &str, s: &str, tif: Tif) -> OrderRequest {
    OrderRequest {
        asset,
        is_buy,
        limit_px: px(p),
        sz: sz(s),
        reduce_only: false,
        kind: OrderKind::Limit { tif },
        cloid: None,
    }
}

pub fn trigger(asset: AssetId, is_buy: bool, p: &str, s: &str, trigger_px: &str, is_market: bool, tpsl: TpSl) -> OrderRequest {
    OrderRequest {
        asset,
        is_buy,
        limit_px: px(p),
        sz: sz(s),
        reduce_only: true,
        kind: OrderKind::Trigger { trigger_px: px(trigger_px), is_market, tpsl },
        cloid: None,
    }
}
