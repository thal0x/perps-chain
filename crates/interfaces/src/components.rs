//! Contracts between the state machine (`crates/state`) and its engines:
//! order book (`crates/book`), clearinghouse (`crates/clearinghouse`),
//! oracle (`crates/oracle`). All are synchronous, single-threaded and
//! deterministic.

use crate::error::{ExecError, InvariantViolation};
use crate::exec::*;
use crate::fixed::*;
use crate::market::*;
use crate::primitives::*;

// ------------------------------------------------------------------ book ---

/// Decision returned by the match hook for each candidate fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchDecision {
    /// Execute the fill (hook has already applied it to the clearinghouse).
    Fill,
    /// Remove the maker order (e.g. maker no longer has margin, self-trade,
    /// reduce-only maker that would increase). Book continues matching.
    CancelMaker(OrderLifecycle),
    /// Stop matching the taker here; the remaining taker size is **always
    /// canceled** (even GTC — resting it would leave a crossed book). Used
    /// when the taker runs out of margin or a reduce-only taker would flip.
    StopTaker,
}

/// A candidate fill offered to the hook. `sz` is the maximum size; the hook
/// may accept a smaller size via `MatchDecision`-free partial: it returns the
/// accepted size in `accept_sz` (0 < accept_sz <= sz) together with `Fill`.
#[derive(Clone, Debug)]
pub struct CandidateFill<'a> {
    pub maker: &'a RestingOrder,
    pub taker_user: Address,
    pub taker_oid: Oid,
    pub taker_side: Side,
    pub taker_reduce_only: bool,
    pub px: Px,
    pub sz: Sz,
}

/// Callback implemented by the state machine; invoked by the book for every
/// potential maker/taker match in price-time priority.
pub trait MatchHook {
    /// Returns (decision, accepted size). For `Fill`, accepted size must be in
    /// (0, candidate.sz]; for other decisions it is ignored. A partial `Fill`
    /// continues matching: the same maker is offered again with updated sizes.
    fn on_match(&mut self, c: &CandidateFill<'_>) -> (MatchDecision, Sz);
}

/// Result of placing a (non-trigger) order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaceOutcome {
    pub filled_sz: Sz,
    /// Volume-weighted average fill price (rounded down for display), if any fill.
    pub avg_px: Option<Px>,
    /// Size left resting in the book (GTC/ALO only).
    pub resting_sz: Sz,
    /// Maker orders removed during matching (with reason).
    pub canceled_makers: Vec<(RestingOrder, OrderLifecycle)>,
    /// Makers that were fully filled and removed.
    pub filled_makers: Vec<Oid>,
    /// Taker remainder canceled (IOC remainder, StopTaker, or ALO rejection).
    pub taker_canceled: bool,
    /// ALO would have crossed -> rejected without matching.
    pub alo_rejected: bool,
}

/// Per-market limit order book with strict price-time priority.
pub trait OrderBook {
    fn asset(&self) -> AssetId;
    /// Match `order` (taker) against the book, then rest the remainder if the
    /// TIF allows. `order.oid` is assigned by the caller. ALO that would cross
    /// is rejected (`alo_rejected`). IOC never rests.
    fn place(&mut self, order: RestingOrder, hook: &mut dyn MatchHook) -> PlaceOutcome;
    fn cancel(&mut self, oid: Oid) -> Option<RestingOrder>;
    fn get(&self, oid: Oid) -> Option<&RestingOrder>;
    fn get_by_cloid(&self, user: &Address, cloid: &Cloid) -> Option<&RestingOrder>;
    fn best_bid(&self) -> Option<Px>;
    fn best_ask(&self) -> Option<Px>;
    /// Aggregated levels, best first: (px, total_sz, n_orders).
    fn levels(&self, side: Side, max_levels: usize) -> Vec<(Px, Sz, u32)>;
    /// All resting orders of a user (deterministic order: by oid).
    fn user_orders(&self, user: &Address) -> Vec<&RestingOrder>;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Canonical serialization for hashing/snapshots (deterministic).
    fn encode(&self) -> Vec<u8>;
}

/// Store of trigger (TP/SL) orders for one market, implemented in `crates/book`.
pub trait TriggerStore {
    fn insert(&mut self, o: TriggerOrder);
    fn remove(&mut self, oid: Oid) -> Option<TriggerOrder>;
    fn get(&self, oid: Oid) -> Option<&TriggerOrder>;
    /// Remove and return all orders whose condition holds at `mark`, in
    /// deterministic order (by oid). Condition: TP sell / SL buy trigger when
    /// mark >= trigger_px; TP buy / SL sell trigger when mark <= trigger_px.
    /// Orders with `parent_oid` still set are not eligible.
    fn take_triggered(&mut self, mark: Px) -> Vec<TriggerOrder>;
    /// Clear `parent_oid` on children of `parent` (parent filled) and return their oids.
    fn release_children(&mut self, parent: Oid) -> Vec<Oid>;
    /// Remove children of `parent` (parent canceled) and return them.
    fn remove_children(&mut self, parent: Oid) -> Vec<TriggerOrder>;
    fn user_orders(&self, user: &Address) -> Vec<&TriggerOrder>;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn encode(&self) -> Vec<u8>;
}

// --------------------------------------------------------- clearinghouse ---

/// Fill as applied to the clearinghouse (both sides at once).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FillParams {
    pub asset: AssetId,
    pub px: Px,
    pub sz: Sz,
    pub taker: Address,
    pub maker: Address,
    pub taker_side: Side,
    /// Liquidation fills skip initial-margin checks for the liquidated side.
    pub liquidation_of: Option<Address>,
    pub time: TimestampMs,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FillResult {
    pub taker_fee: Usd,
    pub maker_fee: Usd,
    pub taker_closed_pnl: Usd,
    pub maker_closed_pnl: Usd,
    pub taker_start_pos: Sz,
    pub maker_start_pos: Sz,
}

/// Margin check request for an order about to be placed or matched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarginCheck {
    pub user: Address,
    pub asset: AssetId,
    pub side: Side,
    pub px: Px,
    pub sz: Sz,
    pub reduce_only: bool,
}

/// Position view (HL clearinghouseState.assetPositions[i].position).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PositionView {
    pub asset: AssetId,
    pub szi: Sz,
    pub entry_px: Px,
    pub position_value: Usd,
    pub unrealized_pnl: Usd,
    pub return_on_equity: Rate,
    pub leverage: Leverage,
    /// Isolated: margin allocated (rawUsd); cross: margin used.
    pub margin_used: Usd,
    pub liquidation_px: Option<Px>,
    pub max_leverage: u32,
    pub cum_funding_all_time: Usd,
    pub cum_funding_since_open: Usd,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct MarginSummary {
    pub account_value: Usd,
    pub total_ntl_pos: Usd,
    pub total_raw_usd: Usd,
    pub total_margin_used: Usd,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AccountView {
    pub margin_summary: MarginSummary,
    pub cross_margin_summary: MarginSummary,
    pub cross_maintenance_margin_used: Usd,
    pub withdrawable: Usd,
    pub positions: Vec<PositionView>,
    /// Spot balances (token index, amount in 1e-6 units), nonzero only,
    /// ascending token index. Token 0 = USDC (separate from perp collateral).
    pub spot_balances: Vec<(u32, Usd)>,
}

/// Per-market context for funding / marks (HL assetCtxs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketCtx {
    pub asset: AssetId,
    pub oracle_px: Px,
    pub mark_px: Px,
    pub mid_px: Option<Px>,
    /// Current hourly funding rate estimate.
    pub funding: Rate,
    pub premium: Rate,
    pub open_interest: Sz,
    pub day_ntl_vlm: Usd,
    pub prev_day_px: Px,
}

/// Liquidation work item produced by the clearinghouse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiquidationTarget {
    pub user: Address,
    /// Positions to close: (asset, size to trade (signed, opposite of position), worst acceptable px).
    pub closes: Vec<(AssetId, Sz, Px)>,
    pub isolated_asset: Option<AssetId>,
}

/// Cross-margin, isolated-margin perp clearinghouse.
///
/// Required invariants (checked by `check_invariants`, property tests, and
/// every block during replay — AC4):
/// 1. Conservation: sum over accounts of (perp collateral + isolated margins +
///    spot USDC (token 0)) + insurance fund + fee pool + funding dust == flows.net(),
///    exactly, where unrealized PnL is not counted (settled cash accounting).
/// 2. Funding nets to zero per market per funding event: sum of payments +
///    rounding dust credited to the insurance fund == 0.
/// 3. After `run_liquidations` + ADL completes for a block, no account has
///    negative equity unless the shortfall was covered (BadDebtCovered event).
pub trait Clearinghouse {
    fn markets(&self) -> &[MarketSpec];

    // --- external flows ---
    fn deposit(&mut self, user: Address, amount: Usd) -> Result<(), ExecError>;
    fn withdraw(&mut self, user: Address, amount: Usd) -> Result<(), ExecError>;
    fn usd_send(&mut self, from: Address, to: Address, amount: Usd) -> Result<(), ExecError>;
    /// Spot token transfer; `token` = index into `Genesis.spot_tokens` (0 = USDC).
    fn spot_send(&mut self, from: Address, to: Address, token: u32, amount: Usd) -> Result<(), ExecError>;
    fn class_transfer(&mut self, user: Address, amount: Usd, to_perp: bool) -> Result<(), ExecError>;

    // --- account settings ---
    fn update_leverage(&mut self, user: Address, asset: AssetId, is_cross: bool, leverage: u32) -> Result<(), ExecError>;
    fn update_isolated_margin(&mut self, user: Address, asset: AssetId, delta: Usd) -> Result<(), ExecError>;
    fn leverage_of(&self, user: &Address, asset: AssetId) -> Leverage;

    // --- trading ---
    /// Pre-trade check: would `user` satisfy initial margin if this order
    /// fully filled at `px` (open orders excluded; HL checks at fill time).
    fn check_order_margin(&self, c: &MarginCheck) -> Result<(), ExecError>;
    /// Reduce-only clamp: max size that reduces the current position.
    fn reduce_only_capacity(&self, user: &Address, asset: AssetId, side: Side) -> Sz;
    /// Apply a fill to both accounts (positions, entry, realized PnL, fees).
    /// Must not partially apply on error.
    fn apply_fill(&mut self, f: &FillParams) -> Result<FillResult, ExecError>;
    fn position(&self, user: &Address, asset: AssetId) -> Sz;

    // --- prices, funding, liquidation ---
    /// Update oracle and mark prices for a market (called every block).
    fn set_prices(&mut self, asset: AssetId, oracle_px: Px, mark_px: Px, mid_px: Option<Px>, now: TimestampMs);
    /// Accrue premium samples; on hourly boundary settle funding. Returns events.
    fn on_block_funding(&mut self, now: TimestampMs) -> Vec<Event>;
    /// Accounts below maintenance margin, deterministic order (by address).
    fn liquidation_targets(&self) -> Vec<LiquidationTarget>;
    /// After market liquidation attempts: close whatever remains of `user`'s
    /// liquidatable positions via insurance-fund backstop, then ADL if the
    /// insurance fund is insufficient. Returns events (ADL fills, bad debt).
    fn backstop_and_adl(&mut self, user: Address, now: TimestampMs) -> Vec<Event>;

    // --- views ---
    fn account_view(&self, user: &Address) -> AccountView;
    fn market_ctx(&self, asset: AssetId) -> MarketCtx;
    fn insurance_fund(&self) -> Usd;
    fn total_collateral(&self) -> Usd;

    // --- integrity ---
    fn check_invariants(&self, flows: &ExternalFlows, height: Height) -> Result<(), InvariantViolation>;
    /// Drain the set of (key, entity hash) changed since the last call.
    fn take_dirty(&mut self) -> Vec<(StateKey, Hash32)>;
    /// Canonical full encoding (snapshots). `decode` must restore exactly.
    fn encode(&self) -> Vec<u8>;
}

// ----------------------------------------------------------------- oracle ---

/// Weighted-median oracle aggregation (pure function family).
pub trait OracleAggregator {
    /// Record a validator's submission at block time `now`.
    fn submit(&mut self, validator: Address, stake: u64, prices: &[(AssetId, Px)], now: TimestampMs);
    /// Stake-weighted median over submissions not older than the freshness
    /// window. Returns None if fresh submissions carry < quorum stake
    /// (caller keeps the previous oracle price).
    fn aggregate(&self, asset: AssetId, now: TimestampMs, quorum_stake: u64) -> Option<Px>;
}

/// Source of external reference prices for a validator's oracle submissions
/// (runs off-chain inside the node; NOT part of the state machine).
pub trait PriceFeed: Send {
    fn prices(&mut self, now_ms: u64) -> Vec<(AssetId, Px)>;
}
