//! `consensus`: sans-IO pipelined 2-chain HotStuff (Jolteon-style) core,
//! crash-safe file block store, threaded driver, and a deterministic
//! network simulator for tests. See README.md.

pub mod config;
pub mod core;
pub mod crypto;
pub mod driver;
pub mod sim;
pub mod store;

pub use crate::config::CoreConfig;
pub use crate::core::{Core, CoreStats};
pub use crate::crypto::{genesis_block, genesis_qc, CertCtx, VoteKind};
pub use crate::driver::{Driver, DriverEvent, DriverHandle, DriverStatus};
pub use crate::store::{FileStore, FileStoreConfig, MemStore};
