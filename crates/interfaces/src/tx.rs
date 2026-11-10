//! Transactions as they appear in blocks.
//!
//! A user transaction stores the **exact msgpack bytes** of the HL action
//! (as produced by `hlwire::action_to_msgpack` from the JSON the SDK posted,
//! preserving key order and integer/string types), because the L1 signing hash
//! is `keccak(msgpack(action) || nonce_be8 || vault_flag[..] || [0x00 || expires_be8])`.
//! The state machine decodes `action_bytes` into a typed [`Action`] with
//! `hlwire::decode_action` and recovers the signer with `hlwire::recover_signer`.

use crate::fixed::*;
use crate::market::*;
use crate::primitives::*;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// Transaction included in a block.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Tx {
    User(UserTx),
    Validator(ValidatorTx),
}

impl Tx {
    pub fn hash(&self) -> TxHash {
        hash_borsh(self)
    }
    pub fn encoded_len(&self) -> usize {
        borsh::to_vec(self).map(|v| v.len()).unwrap_or(0)
    }
}

/// An HL `/exchange` request, signed by a user (or the user's agent wallet).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct UserTx {
    /// msgpack encoding of the action JSON object, key order preserved.
    pub action_bytes: Vec<u8>,
    /// HL nonce (ms timestamp chosen by client). For user-signed actions this
    /// equals the action's `time`/`nonce` field.
    pub nonce: u64,
    pub signature: Sig65,
    pub vault_address: Option<Address>,
    pub expires_after: Option<u64>,
}

/// Transaction signed by a validator's consensus key.
/// Signature = secp256k1 over keccak256(borsh((chain_id, validator, nonce, action))).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct ValidatorTx {
    pub validator: Address,
    /// Strictly increasing per validator.
    pub nonce: u64,
    pub action: ValidatorAction,
    pub signature: Sig65,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum ValidatorAction {
    /// Oracle price submission. Prices are index-aligned with `markets`
    /// ((asset, px) pairs; assets not listed are not updated by this vote).
    OraclePrices { prices: Vec<(AssetId, Px)> },
    /// Attestation that a deposit event was observed (final) on the bridge chain.
    BridgeDeposit(DepositAttestation),
    /// Validator signature over a pending withdrawal (EIP-712, see bridge/).
    WithdrawalSignature { withdrawal_id: u64, signature: Sig65 },
    /// Attestation that a withdrawal was finalized on the bridge chain
    /// (lets the state machine garbage-collect it).
    WithdrawalFinalized { withdrawal_id: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct DepositAttestation {
    /// Monotone deposit id assigned by the bridge contract.
    pub deposit_id: u64,
    pub user: Address,
    pub amount: Usd,
    pub eth_tx_hash: Hash32,
}

/// Typed HL action (result of decoding `UserTx::action_bytes`).
/// Only the subset of HL actions supported by this chain is listed; any other
/// `type` decodes to an error receipt.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum Action {
    Order { orders: Vec<OrderRequest>, grouping: Grouping },
    Cancel { cancels: Vec<(AssetId, Oid)> },
    CancelByCloid { cancels: Vec<(AssetId, Cloid)> },
    Modify { oid: OidOrCloid, order: OrderRequest },
    BatchModify { modifies: Vec<(OidOrCloid, OrderRequest)> },
    /// Dead man's switch: cancel all orders at `time` (None clears it).
    ScheduleCancel { time: Option<u64> },
    UpdateLeverage { asset: AssetId, is_cross: bool, leverage: u32 },
    /// HL `ntli`: signed USD amount (1e-6) to add (+) / remove (-).
    UpdateIsolatedMargin { asset: AssetId, is_buy: bool, ntli: Usd },
    // ---- user-signed (EIP-712 "HyperliquidTransaction:*") ----
    UsdSend { destination: Address, amount: Usd, time: u64 },
    /// Only token "USDC:0x..." is supported.
    SpotSend { destination: Address, token: String, amount: Usd, time: u64 },
    UsdClassTransfer { amount: Usd, to_perp: bool, nonce: u64 },
    Withdraw3 { destination: Address, amount: Usd, time: u64 },
    ApproveAgent { agent_address: Address, agent_name: Option<String>, nonce: u64 },
    /// Accepted and recorded, no fee effect (fee discounts not modeled).
    SetReferrer { code: String },
    /// Invalidates the nonce only.
    Noop,
}

impl Action {
    /// Number of "order actions" this action counts as for throughput
    /// accounting (each order, cancel, or modify counts as one).
    pub fn order_action_count(&self) -> usize {
        match self {
            Action::Order { orders, .. } => orders.len(),
            Action::Cancel { cancels } => cancels.len(),
            Action::CancelByCloid { cancels } => cancels.len(),
            Action::Modify { .. } => 1,
            Action::BatchModify { modifies } => modifies.len(),
            _ => 0,
        }
    }
}
