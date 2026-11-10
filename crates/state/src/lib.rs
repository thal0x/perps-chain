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

/// Chain with fake signatures over the real book/oracle and the fake
/// clearinghouse (used by the tests).
pub type FakeChain = Chain<testkit::BookEngines, testkit::FakeVerifier>;
/// Chain entirely over the simple fakes.
pub type AllFakeChain = Chain<testkit::FakeEngines, testkit::FakeVerifier>;
