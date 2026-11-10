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

    /// `ExternalFlows.genesis`: perp balances + spot USDC + insurance fund.
    fn genesis_total(g: &Genesis) -> Usd {
        let mut t = g.params.insurance_fund;
        for (_, b) in &g.balances {
            t += *b;
        }
        for (_, tok, b) in &g.spot_balances {
            if *tok == 0 {
                t += *b;
            }
        }
        t
    }
    /// Debit a `withdraw3` of gross `amount` with `fee`; returns the value
    /// that leaves the system (added to `flows.withdrawals`). Default: the
    /// whole amount leaves (fee kept by the bridge contract).
    fn withdraw(ch: &mut Self::Ch, user: Address, amount: Usd, _fee: Usd) -> Result<Usd, ExecError> {
        ch.withdraw(user, amount).map(|_| amount)
    }
    /// Drain dirty entity hashes (default: `Clearinghouse::take_dirty`).
    fn take_dirty(ch: &mut Self::Ch) -> Vec<(StateKey, Hash32)> {
        ch.take_dirty()
    }
    /// Every entity hash of a freshly decoded clearinghouse.
    fn all_entity_hashes(ch: &mut Self::Ch) -> Vec<(StateKey, Hash32)> {
        ch.take_dirty()
    }
    /// Extra check after each liquidation pass (invariant mode only).
    fn post_liquidation_check(_ch: &Self::Ch, _height: Height) -> Result<(), InvariantViolation> {
        Ok(())
    }
}

/// The production engines: `clearinghouse::Clearinghouse`, `book::Book`,
/// `book::TriggerBook`, `oracle::OracleState`.
pub struct HlEngines;

impl Engines for HlEngines {
    type Ch = clearinghouse::Clearinghouse;
    type Book = book::Book;
    type Trig = book::TriggerBook;
    type Oracle = oracle::OracleState;
    fn new_clearinghouse(g: &Genesis) -> Self::Ch {
        clearinghouse::Clearinghouse::new(g)
    }
    fn decode_clearinghouse(_g: &Genesis, bytes: &[u8]) -> Result<Self::Ch, String> {
        clearinghouse::Clearinghouse::decode(bytes)
    }
    fn new_book(spec: &MarketSpec) -> Self::Book {
        book::Book::new(spec.asset)
    }
    fn decode_book(_spec: &MarketSpec, bytes: &[u8]) -> Result<Self::Book, String> {
        book::Book::decode(bytes).map_err(|e| e.to_string())
    }
    fn new_triggers(spec: &MarketSpec) -> Self::Trig {
        book::TriggerBook::new(spec.asset)
    }
    fn decode_triggers(_spec: &MarketSpec, bytes: &[u8]) -> Result<Self::Trig, String> {
        book::TriggerBook::decode(bytes).map_err(|e| e.to_string())
    }
    fn new_oracle(g: &Genesis) -> Self::Oracle {
        oracle::OracleState::from_genesis(g)
    }
    fn genesis_total(g: &Genesis) -> Usd {
        clearinghouse::Clearinghouse::genesis_total(g)
    }
    fn withdraw(ch: &mut Self::Ch, user: Address, amount: Usd, fee: Usd) -> Result<Usd, ExecError> {
        ch.withdraw_with_fee(user, amount, fee).map(|_| amount - fee)
    }
    fn all_entity_hashes(ch: &mut Self::Ch) -> Vec<(StateKey, Hash32)> {
        let mut v = ch.all_entity_hashes();
        v.extend(ch.take_dirty());
        v
    }
    fn post_liquidation_check(ch: &Self::Ch, height: Height) -> Result<(), InvariantViolation> {
        let bad = ch.negative_equity_accounts();
        if bad.is_empty() {
            Ok(())
        } else {
            Err(InvariantViolation { height, what: format!("negative equity after liquidation pass: {bad:?}") })
        }
    }
}
