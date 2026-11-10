//! Factories binding the state machine to concrete engine implementations.
//!
//! The interface traits (`OrderBook`, `TriggerStore`, `Clearinghouse`,
//! `OracleAggregator`) describe behaviour but not construction or decoding
//! (snapshots need both), so the chain is generic over an [`Engines`] bundle
//! that supplies them. See `INTERFACE_REQUESTS.md`.

use interfaces::*;

/// Bundle of engine types plus their constructors / snapshot decoders.
///
/// Contract for `new_clearinghouse` and `decode_clearinghouse`: the returned
/// value's first `take_dirty()` must report **every** entity it owns, so the
/// state crate can rebuild the full entity-hash map (genesis and restore).
pub trait Engines: 'static {
    type Ch: Clearinghouse + Send + Sync;
    type Book: OrderBook + Send + Sync;
    type Trig: TriggerStore + Send + Sync;
    type Oracle: OracleAggregator + Send + Sync;

    /// Clearinghouse at genesis: credits `g.balances`, `g.spot_balances` and
    /// the insurance fund `g.params.insurance_fund`.
    fn new_clearinghouse(g: &Genesis) -> Self::Ch;
    fn decode_clearinghouse(g: &Genesis, bytes: &[u8]) -> Result<Self::Ch, String>;
    fn new_book(spec: &MarketSpec) -> Self::Book;
    fn decode_book(spec: &MarketSpec, bytes: &[u8]) -> Result<Self::Book, String>;
    fn new_triggers(spec: &MarketSpec) -> Self::Trig;
    fn decode_triggers(spec: &MarketSpec, bytes: &[u8]) -> Result<Self::Trig, String>;
    /// Empty oracle. Oracle state is mirrored by the chain (latest submission
    /// per (validator, asset)) and replayed into a fresh oracle on restore.
    fn new_oracle(g: &Genesis) -> Self::Oracle;
}
