//! Genesis / chain parameters. Loaded from `genesis.json` by every node.

use crate::block::ValidatorSet;
use crate::fixed::*;
use crate::market::MarketSpec;
use crate::primitives::*;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct Genesis {
    pub chain_id: u64,
    /// false => SDK "testnet" signing (phantom agent source "b",
    /// hyperliquidChain "Testnet"). Local networks use false because the SDKs
    /// derive `is_mainnet` from the base URL.
    pub is_mainnet: bool,
    pub genesis_time_ms: TimestampMs,
    pub validators: ValidatorSet,
    pub markets: Vec<MarketSpec>,
    /// Perp collateral credited at genesis (test accounts).
    pub balances: Vec<(Address, Usd)>,
    /// Spot tokens; index 0 must be USDC. Spot trading is not supported, only
    /// balances and transfers (spotSend, usdClassTransfer for USDC).
    pub spot_tokens: Vec<SpotToken>,
    /// Spot balances at genesis: (user, token index, amount 1e-6 units).
    pub spot_balances: Vec<(Address, u32, Usd)>,
    pub params: ChainParams,
    pub bridge: BridgeParams,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct ChainParams {
    /// Taker fee (Rate, e.g. 0.00035 = 350_000_000).
    pub taker_fee: Rate,
    /// Maker fee; negative = rebate.
    pub maker_fee: Rate,
    /// Funding settlement interval (HL: 1h).
    pub funding_interval_ms: u64,
    /// Max |hourly funding rate| (HL: 4%/h).
    pub funding_cap: Rate,
    /// Hourly interest component (HL: 0.01%/8h => 0.00125%/h).
    pub funding_interest: Rate,
    /// Oracle submissions older than this are ignored.
    pub oracle_freshness_ms: u64,
    /// Insurance fund balance at genesis.
    pub insurance_fund: Usd,
    /// Withdrawal fee (HL: 1 USDC).
    pub withdraw_fee: Usd,
    /// Max txs / bytes per block.
    pub max_block_txs: u32,
    pub max_block_bytes: u32,
    /// Max order actions per block.
    pub max_block_order_actions: u32,
    /// Max liquidation slippage vs mark for market liquidation orders.
    pub liquidation_slippage: Rate,
}

impl Default for ChainParams {
    fn default() -> Self {
        Self {
            taker_fee: Rate::from_raw(350_000_000),   // 0.035%
            maker_fee: Rate::from_raw(100_000_000),   // 0.01%
            funding_interval_ms: 3_600_000,
            funding_cap: Rate::from_raw(40_000_000_000), // 4%
            funding_interest: Rate::from_raw(12_500_000), // 0.00125%
            oracle_freshness_ms: 10_000,
            insurance_fund: Usd::from_raw(1_000_000 * USD_SCALE),
            withdraw_fee: Usd::from_raw(USD_SCALE),
            max_block_txs: 20_000,
            max_block_bytes: 16 * 1024 * 1024,
            max_block_order_actions: 400_000,
            liquidation_slippage: Rate::from_raw(20_000_000_000), // 2%
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct BridgeParams {
    /// EVM chain id of the bridge chain (Anvil default 31337).
    pub eth_chain_id: u64,
    /// Bridge contract address on the EVM chain.
    pub contract: Address,
    /// Minimum deposit amount accepted (mirrors Bridge.sol `minDeposit`).
    /// Deposits are credited at quorum stake of identical attestations.
    pub min_deposit: Usd,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct SpotToken {
    pub index: u32,
    /// e.g. "USDC", "PURR".
    pub name: String,
    pub sz_decimals: u32,
    pub wei_decimals: u32,
    /// HL tokenId (16-byte hex, used in "NAME:0x<tokenId>" token strings).
    pub token_id: [u8; 16],
}
