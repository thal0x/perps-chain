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
pub mod testkit;
pub mod views;

pub use chain::Chain;
pub use config::*;
pub use engines::Engines;
pub use sigcache::SigCache;

/// Chain over the test fakes (fake signatures, simple engines).
pub type FakeChain = Chain<testkit::FakeEngines, testkit::FakeVerifier>;
