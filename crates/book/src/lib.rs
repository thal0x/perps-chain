//! Per-market limit order book (`Book`, implements `interfaces::OrderBook`)
//! and trigger-order store (`TriggerBook`, implements
//! `interfaces::TriggerStore`). Pure, single-threaded, deterministic, integer
//! only. See README.md for semantics.

mod book;
mod fxhash;
mod trigger;

pub use book::{Book, BookDecodeError, NotionalAcc};
pub use fxhash::{FxHasher, FxMap};
pub use trigger::{condition_holds, fires_on_rise, TriggerBook, TriggerDecodeError};
