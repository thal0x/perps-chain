//! Execution outputs: receipts (the `/exchange` response), events (feed the
//! API indexer and WebSocket), and state-commitment keys.

use crate::fixed::*;
use crate::market::*;
use crate::primitives::*;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// Context for executing one committed block.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct BlockContext {
    pub height: Height,
    pub timestamp_ms: TimestampMs,
    pub proposer: Address,
    pub block_id: BlockId,
}

/// Per-order status in an `order` response (HL `statuses[i]`).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum OrderStatusResp {
    Resting { oid: Oid, cloid: Option<Cloid> },
    Filled { total_sz: Sz, avg_px: Px, oid: Oid, cloid: Option<Cloid> },
    /// Trigger orders and grouped children that are parked.
    WaitingForTrigger { oid: Oid },
    WaitingForFill { oid: Oid },
    Error(String),
}

/// Per-entry status in a cancel / modify response (HL "success" or {"error": ..}).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum SimpleStatus {
    Success,
    Error(String),
}

/// Typed `/exchange` response body for status "ok".
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum ActionResponse {
    /// `{"type":"order","data":{"statuses":[..]}}`; also used for modify/batchModify.
    Order(Vec<OrderStatusResp>),
    /// `{"type":"cancel","data":{"statuses":[..]}}`
    Cancel(Vec<SimpleStatus>),
    /// `{"type":"default"}`
    Default,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct TxReceipt {
    pub tx_hash: TxHash,
    /// Recovered principal (user, after agent resolution); ZERO if signature invalid.
    pub user: Address,
    /// Ok(body) => HL `{"status":"ok","response":{...}}`;
    /// Err(msg) => HL `{"status":"err","response":msg}`.
    pub result: Result<ActionResponse, String>,
}

/// HL order lifecycle status names (orderUpdates / orderStatus).
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum OrderLifecycle {
    Open,
    Filled,
    Canceled,
    Triggered,
    Rejected,
    MarginCanceled,
    ReduceOnlyCanceled,
    SelfTradeCanceled,
    ScheduledCancel,
    LiquidatedCanceled,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct FillEvent {
    pub asset: AssetId,
    pub tid: Tid,
    pub px: Px,
    pub sz: Sz,
    pub time: TimestampMs,
    pub tx_hash: TxHash,
    pub taker: Address,
    pub maker: Address,
    pub taker_oid: Oid,
    pub maker_oid: Oid,
    pub taker_side: Side,
    pub taker_fee: Usd,
    /// Negative = rebate.
    pub maker_fee: Usd,
    pub taker_closed_pnl: Usd,
    pub maker_closed_pnl: Usd,
    /// Signed position before the fill.
    pub taker_start_pos: Sz,
    pub maker_start_pos: Sz,
    pub taker_cloid: Option<Cloid>,
    pub maker_cloid: Option<Cloid>,
    /// Set if this fill is part of a liquidation of `liquidated_user`.
    pub liquidation: Option<LiquidationInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct LiquidationInfo {
    pub liquidated_user: Address,
    pub mark_px: Px,
    pub method: LiquidationMethod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum LiquidationMethod {
    Market,
    Backstop,
    Adl,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum LedgerDelta {
    Deposit { usdc: Usd },
    Withdraw { usdc: Usd, nonce: u64, fee: Usd },
    InternalTransfer { usdc: Usd, user: Address, destination: Address, fee: Usd },
    SpotTransfer { token: String, amount: Usd, user: Address, destination: Address, fee: Usd },
    AccountClassTransfer { usdc: Usd, to_perp: bool },
    Liquidation { account_value: Usd, liquidated_ntl_pos: Usd },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum Event {
    Fill(FillEvent),
    OrderUpdate {
        user: Address,
        order: RestingOrderView,
        status: OrderLifecycle,
        time: TimestampMs,
    },
    Funding {
        user: Address,
        asset: AssetId,
        /// Positive = received.
        usdc: Usd,
        szi: Sz,
        funding_rate: Rate,
        time: TimestampMs,
    },
    Ledger { user: Address, time: TimestampMs, tx_hash: TxHash, delta: LedgerDelta },
    /// Book for `asset` changed during this block (WS l2Book / bbo push).
    BookChanged { asset: AssetId },
    /// Insurance fund / ADL activity (for diagnostics and invariants).
    Adl { asset: AssetId, user: Address, counterparty: Address, sz: Sz, px: Px },
    BadDebtCovered { asset: AssetId, user: Address, amount: Usd, by_insurance: bool },
}

/// Order view used in events and queries (HL `order` object in orderUpdates /
/// frontendOpenOrders).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct RestingOrderView {
    pub asset: AssetId,
    pub oid: Oid,
    pub side: Side,
    pub limit_px: Px,
    pub sz: Sz,
    pub orig_sz: Sz,
    pub timestamp: TimestampMs,
    pub cloid: Option<Cloid>,
    pub reduce_only: bool,
    pub order_type: String,
    pub tif: Option<Tif>,
    pub trigger: Option<(Px, bool, TpSl)>,
    pub is_trigger: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct BlockOutcome {
    pub height: Height,
    pub state_root: Hash32,
    /// One receipt per *user* tx in payload order (validator txs produce none).
    pub receipts: Vec<TxReceipt>,
    pub events: Vec<Event>,
    /// Number of order actions executed (throughput accounting).
    pub order_actions: u64,
}

/// Keys of the state commitment. The state root is
/// `blake3( concat over keys in ascending order of (borsh(key) || entity_hash) )`
/// maintained incrementally: only entities marked dirty in a block are
/// re-hashed. See ARCHITECTURE.md §State commitment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, BorshSerialize, BorshDeserialize)]
pub enum StateKey {
    /// Global counters, params, time, next oid/tid, etc.
    Global,
    /// Per-user clearinghouse account (balances, positions, leverage).
    Account(Address),
    /// Per-user non-clearinghouse data (nonces, agents, scheduled cancel).
    UserMeta(Address),
    /// Per-market state (funding accumulators, mark, OI).
    Market(AssetId),
    /// Order book (resting + trigger orders) of a market.
    Book(AssetId),
    /// Oracle submissions and aggregates.
    Oracle,
    /// Bridge: processed deposits, pending withdrawals.
    Bridge,
    /// Insurance fund and fee pool.
    Insurance,
}

/// Running totals of value entering/leaving the clearinghouse, used by the
/// conservation invariant (AC4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct ExternalFlows {
    pub deposits: Usd,
    pub withdrawals: Usd,
    /// Genesis allocations.
    pub genesis: Usd,
}

impl ExternalFlows {
    /// Value that must be held inside the system.
    pub fn net(&self) -> Usd {
        self.genesis + self.deposits - self.withdrawals
    }
}
