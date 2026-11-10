//! Shared types and traits at every crate boundary.
//!
//! Owned by the lead. Changes go through DECISIONS.md.

pub mod block;
pub mod components;
pub mod error;
pub mod exec;
pub mod fixed;
pub mod genesis;
pub mod market;
pub mod primitives;
pub mod traits;
pub mod tx;

pub use block::*;
pub use components::*;
pub use error::*;
pub use exec::*;
pub use fixed::*;
pub use genesis::*;
pub use market::*;
pub use primitives::*;
pub use traits::*;
pub use tx::*;

/// Interface revision; bump on every DECISIONS.md interface change.
pub const INTERFACES_REV: u32 = 1;
