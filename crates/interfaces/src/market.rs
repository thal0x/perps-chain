//! Market and order types (typed form of HL wire orders).

use crate::fixed::*;
use crate::primitives::*;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// Static description of a perp market. Lives in genesis; index in the
/// `markets` vector == `asset` == HL universe index.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct MarketSpec {
    pub asset: AssetId,
    /// HL coin name, e.g. "BTC".
    pub name: String,
    /// Sizes must be multiples of 10^-sz_decimals.
    pub sz_decimals: u32,
    /// Max leverage (integer, HL "maxLeverage").
    pub max_leverage: u32,
    /// Only isolated margin allowed.
    pub only_isolated: bool,
    /// Initial oracle price used until validators submit prices.
    pub initial_px: Px,
}

impl MarketSpec {
    /// HL price rule: at most 5 significant figures (integers always allowed)
    /// and at most `6 - sz_decimals` decimals.
    pub fn max_px_decimals(&self) -> u32 {
        6u32.saturating_sub(self.sz_decimals)
    }
    pub fn is_valid_px(&self, px: Px) -> bool {
        px.0 > 0
            && px.has_at_most_decimals(self.max_px_decimals())
            && (px_sig_figs(px) <= 5 || px.has_at_most_decimals(0))
    }
    pub fn is_valid_sz(&self, sz: Sz) -> bool {
        sz.0 > 0 && sz.has_at_most_decimals(self.sz_decimals)
    }
    /// Maintenance margin fraction = 1 / (2 * max_leverage) (HL rule).
    pub fn maintenance_margin_rate(&self) -> Rate {
        Rate::from_raw(RATE_SCALE / (2 * self.max_leverage as i128))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum Side {
    /// Buy. HL "B".
    Bid,
    /// Sell. HL "A".
    Ask,
}

impl Side {
    pub fn from_is_buy(b: bool) -> Self {
        if b {
            Side::Bid
        } else {
            Side::Ask
        }
    }
    pub fn is_buy(self) -> bool {
        self == Side::Bid
    }
    pub fn opposite(self) -> Self {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }
    /// +1 for buys, -1 for sells.
    pub fn sign(self) -> i64 {
        match self {
            Side::Bid => 1,
            Side::Ask => -1,
        }
    }
    pub fn hl_str(self) -> &'static str {
        match self {
            Side::Bid => "B",
            Side::Ask => "A",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum Tif {
    Gtc,
    Ioc,
    /// Add-liquidity-only (post only): rejected if it would cross.
    Alo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum TpSl {
    Tp,
    Sl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum OrderKind {
    Limit { tif: Tif },
    /// Trigger order: activates when mark price crosses `trigger_px`; then
    /// becomes a market (IOC with slippage) or limit (GTC at limit_px) order.
    Trigger { trigger_px: Px, is_market: bool, tpsl: TpSl },
}

/// Typed order request (HL OrderWire after parsing).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct OrderRequest {
    pub asset: AssetId,
    pub is_buy: bool,
    pub limit_px: Px,
    pub sz: Sz,
    pub reduce_only: bool,
    pub kind: OrderKind,
    pub cloid: Option<Cloid>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum Grouping {
    Na,
    NormalTpsl,
    PositionTpsl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum OidOrCloid {
    Oid(Oid),
    Cloid(Cloid),
}

/// Order as it rests in the book.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct RestingOrder {
    pub oid: Oid,
    pub user: Address,
    pub asset: AssetId,
    pub side: Side,
    pub limit_px: Px,
    /// Remaining size (> 0).
    pub sz: Sz,
    /// Original size at placement.
    pub orig_sz: Sz,
    pub reduce_only: bool,
    pub cloid: Option<Cloid>,
    /// Block timestamp at placement (HL "timestamp").
    pub timestamp: TimestampMs,
    pub tif: Tif,
}

/// Trigger order waiting for activation (not in the book).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct TriggerOrder {
    pub oid: Oid,
    pub user: Address,
    pub asset: AssetId,
    pub side: Side,
    pub limit_px: Px,
    pub sz: Sz,
    pub reduce_only: bool,
    pub cloid: Option<Cloid>,
    pub timestamp: TimestampMs,
    pub trigger_px: Px,
    pub is_market: bool,
    pub tpsl: TpSl,
    /// For grouped TP/SL: parent order id that must fill first.
    pub parent_oid: Option<Oid>,
    /// positionTpsl: size follows the position at trigger time.
    pub is_position_tpsl: bool,
}

/// Leverage setting for an (account, asset) pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum Leverage {
    Cross(u32),
    Isolated(u32),
}

impl Leverage {
    pub fn value(&self) -> u32 {
        match self {
            Leverage::Cross(v) | Leverage::Isolated(v) => *v,
        }
    }
    pub fn is_cross(&self) -> bool {
        matches!(self, Leverage::Cross(_))
    }
}
