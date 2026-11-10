//! `hlwire`: Hyperliquid wire compatibility.
//!
//! * [`json_to_msgpack`] / [`msgpack_to_json`]: byte-identical to Python
//!   `msgpack.packb` and `@std/msgpack` for HL action objects.
//! * [`decode_action`]: HL wire action -> typed [`interfaces::Action`].
//! * L1 phantom-agent signing ([`l1_action_hash`], [`l1_signing_digest`],
//!   [`sign_l1_action`]) and user-signed EIP-712 ([`user_signed_digest`],
//!   [`sign_user_signed_action`]).
//! * [`parse_exchange_request`] / [`build_exchange_request`] for `/exchange`.
//! * [`HlVerifier`] implementing [`interfaces::TxVerifier`].
//! * [`Wallet`], [`recover`], validator-tx and consensus digests.
//! * SDK-shaped action builders for the simulator ([`order_action`], ...).
//!
//! See `WIRE.md` for the exact JSON shapes.

mod build;
mod crypto;
mod decode;
mod fast;
mod msgpack;
mod sign;

pub use build::*;
pub use crypto::*;
pub use decode::*;
pub use msgpack::{action_to_msgpack, json_to_msgpack, msgpack_to_json};
pub use sign::*;

pub use interfaces::Sig65;
