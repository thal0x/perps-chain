//! Persistent (committed) state types. Everything here is borsh-encoded into
//! entity hashes and snapshots; field order is part of the commitment.

use borsh::{BorshDeserialize, BorshSerialize};
use interfaces::fixed::*;
use interfaces::market::*;
use interfaces::primitives::*;

/// Address of the insurance fund pseudo-account. It is a normal account
/// internally (it can hold positions taken over in backstop liquidations)
/// but is committed under `StateKey::Insurance` and can never be used by a
/// user action.
pub const INSURANCE: Address = Address([0xFF; 20]);

/// One open perp position.
///
/// Cash accounting (see README): the account's cash buckets are moved by the
/// full fill notional at fill time (the buyer pays `N`, the seller receives
/// `N`). `basis` is the signed cost basis of the open size (long > 0) and is
/// only used for entry price / realized-PnL *reporting*; equity is always
/// `cash + szi * mark`.
#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Position {
    pub asset: AssetId,
    pub szi: Sz,
    /// Signed cost basis (sum of notionals of the open size).
    pub basis: Usd,
    /// Isolated cash bucket (HL `rawUsd` of an isolated position); 0 for cross.
    pub iso_cash: Usd,
    /// Funding paid (positive) since the position was opened (or flipped).
    pub cum_funding_since_open: Usd,
    /// Funding paid since the size last changed.
    pub cum_funding_since_change: Usd,
}

/// Per (account, asset) settings and lifetime counters.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct AssetSetting {
    pub asset: AssetId,
    pub leverage: Leverage,
    /// Funding paid (positive) over the account's lifetime on this asset.
    pub cum_funding_all_time: Usd,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct AccountState {
    pub address: Address,
    /// Cross cash (HL cross `rawUsd`). May be negative while cross positions
    /// are open (it already paid/received the positions' notional).
    pub cash: Usd,
    /// Negative isolated cash left after an isolated position was closed by
    /// liquidation below bankruptcy; covered by the insurance fund in
    /// `backstop_and_adl`. Always <= 0.
    pub iso_deficit: Usd,
    /// Open positions, ascending asset, szi != 0.
    pub positions: Vec<Position>,
    /// Ascending asset.
    pub settings: Vec<AssetSetting>,
    /// Spot balances (token, amount), ascending token, nonzero.
    pub spot: Vec<(u32, Usd)>,
}

impl AccountState {
    pub fn new(address: Address) -> Self {
        Self { address, ..Default::default() }
    }
    pub fn pos(&self, asset: AssetId) -> Option<&Position> {
        self.positions.binary_search_by_key(&asset, |p| p.asset).ok().map(|i| &self.positions[i])
    }
    pub fn szi(&self, asset: AssetId) -> Sz {
        self.pos(asset).map(|p| p.szi).unwrap_or(Sz::ZERO)
    }
    /// Insert/replace (szi != 0) or remove (szi == 0) a position.
    pub fn put_pos(&mut self, p: Position) {
        match self.positions.binary_search_by_key(&p.asset, |q| q.asset) {
            Ok(i) => {
                if p.szi.is_zero() {
                    self.positions.remove(i);
                } else {
                    self.positions[i] = p;
                }
            }
            Err(i) => {
                if !p.szi.is_zero() {
                    self.positions.insert(i, p);
                }
            }
        }
    }
    pub fn setting(&self, asset: AssetId) -> Option<&AssetSetting> {
        self.settings.binary_search_by_key(&asset, |s| s.asset).ok().map(|i| &self.settings[i])
    }
    pub fn setting_mut(&mut self, asset: AssetId, default_lev: Leverage) -> &mut AssetSetting {
        let i = match self.settings.binary_search_by_key(&asset, |s| s.asset) {
            Ok(i) => i,
            Err(i) => {
                self.settings.insert(
                    i,
                    AssetSetting { asset, leverage: default_lev, cum_funding_all_time: Usd::ZERO },
                );
                i
            }
        };
        &mut self.settings[i]
    }
    pub fn spot_bal(&self, token: u32) -> Usd {
        self.spot
            .binary_search_by_key(&token, |s| s.0)
            .ok()
            .map(|i| self.spot[i].1)
            .unwrap_or(Usd::ZERO)
    }
    pub fn spot_add(&mut self, token: u32, delta: Usd) {
        match self.spot.binary_search_by_key(&token, |s| s.0) {
            Ok(i) => {
                let v = self.spot[i].1 + delta;
                assert!(v.0 >= 0, "negative spot balance");
                if v.is_zero() {
                    self.spot.remove(i);
                } else {
                    self.spot[i].1 = v;
                }
            }
            Err(i) => {
                assert!(delta.0 >= 0, "negative spot balance");
                if !delta.is_zero() {
                    self.spot.insert(i, (token, delta));
                }
            }
        }
    }
}

/// Per-market dynamic state (committed under `StateKey::Market(asset)`).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct MarketState {
    pub oracle_px: Px,
    pub mark_px: Px,
    pub mid_px: Option<Px>,
    /// Sum of premium samples (Rate raw) since the last settlement.
    pub premium_sum: i128,
    pub premium_samples: u64,
    pub last_premium: Rate,
    /// `time / 5000` of the last premium sample.
    pub last_sample_slot: u64,
    /// `time / funding_interval_ms` of the last settlement.
    pub last_funding_slot: u64,
    pub last_funding_rate: Rate,
    pub last_funding_time: TimestampMs,
    pub oi_long: Sz,
    pub oi_short: Sz,
    pub day: u64,
    pub day_ntl_vlm: Usd,
    pub prev_day_px: Px,
    /// Lifetime funding totals: paid by payers, received by receivers, and
    /// rounding dust credited to the insurance fund. paid == received + dust.
    pub funding_paid: Usd,
    pub funding_received: Usd,
    pub funding_dust: Usd,
}

/// Global clearinghouse ledger items committed under `StateKey::Insurance`
/// together with the insurance account.
#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Ledger {
    /// Trading fees collected (taker + maker fees − maker rebates) and
    /// withdrawal fees.
    pub fee_pool: Usd,
    /// Total funding rounding dust credited to the insurance fund.
    pub funding_dust_total: Usd,
    /// Bad debt paid by the insurance fund.
    pub bad_debt_insurance: Usd,
    /// Bad debt socialized through ADL (bankruptcy-price closes).
    pub bad_debt_adl: Usd,
    /// Genesis totals per spot token (index = token). Non-USDC tokens have
    /// no external flows, so Σ balances must stay equal to this.
    pub spot_supply: Vec<Usd>,
}

/// Full canonical encoding (snapshots).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct ChSnapshot {
    pub version: u32,
    pub specs: Vec<MarketSpec>,
    pub params: interfaces::genesis::ChainParams,
    pub spot_tokens: Vec<interfaces::genesis::SpotToken>,
    pub markets: Vec<MarketState>,
    /// Ascending address (insurance included).
    pub accounts: Vec<AccountState>,
    pub ledger: Ledger,
}
