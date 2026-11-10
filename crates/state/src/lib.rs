//! hlbft replicated state machine: executes committed blocks by driving the
//! order books, clearinghouse, oracle and bridge bookkeeping. See README.md.

pub mod chain;
pub mod config;
pub mod crypto;
pub mod engines;
pub mod errors;
mod hook;
pub mod meta;
mod query;
pub mod sigcache;
pub mod spot;
pub mod testkit;
pub mod views;

pub use chain::Chain;
pub use config::*;
pub use engines::{Engines, HlEngines};
pub use sigcache::SigCache;
pub use spot::{SpotHold, SpotPairSpec, SPOT_ASSET, SPOT_FEE_ACCOUNT};

/// Chain with fake signatures over the real book/oracle and the fake
/// clearinghouse (used by the tests).
pub type FakeChain = Chain<testkit::BookEngines, testkit::FakeVerifier>;
/// Chain entirely over the simple fakes.
pub type AllFakeChain = Chain<testkit::FakeEngines, testkit::FakeVerifier>;
/// The production chain: real engines + HL signature scheme.
pub type HlChain = Chain<HlEngines, hlwire::HlVerifier>;

impl HlChain {
    /// Production constructor (verifier from `genesis.is_mainnet`).
    pub fn new_hl(genesis: interfaces::Genesis, cfg: ChainConfig) -> Self {
        let v = std::sync::Arc::new(hlwire::HlVerifier::new(genesis.is_mainnet));
        Chain::new(genesis, v, cfg)
    }
}

/// Production engines with a pluggable verifier (`hlwire::HlVerifier` in the node).
pub type HlChainWith<V> = Chain<HlEngines, V>;
/// Real engines with fake signatures (tests, simulations without ECDSA).
pub type TestChain = Chain<HlEngines, testkit::FakeVerifier>;
