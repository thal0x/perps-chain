//! Error types that cross crate boundaries.

use crate::primitives::*;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    #[error("empty string")]
    Empty,
    #[error("bad hex")]
    BadHex,
    #[error("bad length: expected {expected}, got {got}")]
    BadLength { expected: usize, got: usize },
    #[error("bad decimal: {0}")]
    BadDecimal(String),
    #[error("too much precision: {0}")]
    TooPrecise(String),
    #[error("numeric overflow")]
    Overflow,
    #[error("{0}")]
    Other(String),
}

/// Reason a transaction was rejected before entering a block (mempool / API).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AdmissionError {
    #[error("invalid signature")]
    BadSignature,
    #[error("malformed transaction: {0}")]
    Malformed(String),
    #[error("nonce rejected: {0}")]
    BadNonce(String),
    #[error("rate limited")]
    RateLimited,
    #[error("mempool full")]
    MempoolFull,
    #[error("duplicate transaction")]
    Duplicate,
    #[error("unknown signer {0}")]
    UnknownSigner(Address),
    #[error("{0}")]
    Other(String),
}

/// Failure that aborts a whole transaction during execution. The tx is still
/// included in the block (nonce consumed if signature valid) and a receipt
/// with `status: "err"` is produced. Error strings must match HL texts where
/// HL has one (see crates/api/HL_ERRORS.md).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0}")]
pub struct ExecError(pub String);

impl ExecError {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

/// Invariant violation detected by a component self-check. Any occurrence is
/// a bug; replay/acceptance runs abort on it.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invariant violated at height {height}: {what}")]
pub struct InvariantViolation {
    pub height: Height,
    pub what: String,
}
