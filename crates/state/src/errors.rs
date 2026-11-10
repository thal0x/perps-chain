//! HL error strings returned in receipts / order statuses.
//! Keep in sync with `crates/state/HL_ERRORS.md`.

pub const MIN_VALUE: &str = "Order must have minimum value of $10.";
pub const IOC_NO_MATCH: &str = "Order could not immediately match against any resting orders.";
pub const INSUFFICIENT_MARGIN: &str = "Insufficient margin to place order.";
pub const REDUCE_ONLY_INCREASE: &str = "Reduce only order would increase position.";
pub const INVALID_PRICE: &str = "Order has invalid price.";
pub const INVALID_SIZE: &str = "Order has invalid size.";
pub const INVALID_TPSL_PRICE: &str = "Invalid TP/SL price.";
pub const CANCEL_MISSING: &str = "Order was never placed, already canceled, or filled.";
pub const MODIFY_MISSING: &str = "Cannot modify canceled or filled order";
pub const INVALID_ASSET: &str = "Invalid asset.";
pub const TPSL_PARENT_FAILED: &str = "Parent order failed.";
pub const TPSL_CHILD_NOT_TRIGGER: &str = "TP/SL child order must be a trigger order.";
pub const TPSL_ASSET_MISMATCH: &str = "TP/SL child must have the same asset as the parent order.";
pub const POSITION_TPSL_NOT_TRIGGER: &str = "positionTpsl orders must be trigger orders.";
pub const SCHEDULE_TOO_EARLY: &str = "Scheduled cancel time too early, must be at least 5 seconds after current time.";
pub const INVALID_LEVERAGE: &str = "Invalid leverage value.";
pub const EXPIRED: &str = "Expired";
pub const VAULT_UNSUPPORTED: &str = "Vault actions are not supported.";
pub const INVALID_SIGNATURE: &str = "Invalid signature.";
pub const INVALID_AMOUNT: &str = "Invalid amount.";
pub const INVALID_TOKEN: &str = "Invalid token.";
pub const WITHDRAW_TOO_SMALL: &str = "Withdrawal amount must be greater than the withdrawal fee.";
pub const AGENT_IN_USE: &str = "Agent address is already in use.";
pub const AGENT_IS_SELF: &str = "Cannot approve self as agent.";
pub const REFERRER_ALREADY_SET: &str = "Referrer already set.";
pub const DUPLICATE_CLOID: &str = "Duplicate cloid.";
pub const NONCE_MISMATCH: &str = "Invalid nonce: action time does not match request nonce.";

pub fn post_only_crossed(bid: &str, ask: &str) -> String {
    format!("Post only order would have immediately matched, bbo was {bid}@{ask}")
}

pub fn user_does_not_exist(addr: &interfaces::Address) -> String {
    format!("User or API Wallet {addr} does not exist.")
}
