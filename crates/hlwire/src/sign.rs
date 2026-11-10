//! HL signing schemes (L1 phantom agent + user-signed EIP-712), `/exchange`
//! request parsing/building, and the [`HlVerifier`] used by the state machine.

use crate::crypto::{keccak256, keccak256_concat, recover, recover_validator_tx, Wallet};
use crate::decode::{decode_action, parse_address};
use crate::msgpack::json_to_msgpack;
use interfaces::*;
use serde_json::{Map, Value};
use std::sync::OnceLock;

/// `signatureChainId` both SDKs' Python flavour uses (Arbitrum Sepolia-ish id
/// "0x66eee"); the TS SDK defaults to the wallet's chain ("0x1" for local
/// viem accounts). Any value is accepted; it only feeds the EIP-712 domain.
pub const DEFAULT_SIGNATURE_CHAIN_ID: &str = "0x66eee";

const EIP712_DOMAIN_TYPE: &[u8] = b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";

fn u256_be(v: u64) -> [u8; 32] {
    let mut o = [0u8; 32];
    o[24..].copy_from_slice(&v.to_be_bytes());
    o
}

fn domain_separator(name: &str, chain_id: u64) -> [u8; 32] {
    keccak256_concat(&[
        &keccak256(EIP712_DOMAIN_TYPE),
        &keccak256(name.as_bytes()),
        &keccak256(b"1"),
        &u256_be(chain_id),
        &[0u8; 32], // verifyingContract = 0x0
    ])
}

fn eip712_digest(domain_sep: &[u8; 32], struct_hash: &[u8; 32]) -> [u8; 32] {
    keccak256_concat(&[b"\x19\x01", domain_sep, struct_hash])
}

// ------------------------------------------------------------------ L1

/// `keccak(msgpack(action) || nonce_be8 || (0x00 | 0x01 || vault20) || [0x00 || expires_be8])`.
pub fn l1_action_hash(action_bytes: &[u8], nonce: u64, vault: Option<Address>, expires_after: Option<u64>) -> [u8; 32] {
    use sha3::{Digest, Keccak256};
    let mut h = Keccak256::new();
    h.update(action_bytes);
    h.update(nonce.to_be_bytes());
    match vault {
        None => h.update([0u8]),
        Some(a) => {
            h.update([1u8]);
            h.update(a.0);
        }
    }
    if let Some(e) = expires_after {
        h.update([0u8]);
        h.update(e.to_be_bytes());
    }
    h.finalize().into()
}

/// EIP-712 digest of `Agent{source, connectionId}` in domain
/// `Exchange / 1 / 1337 / 0x0`; source "a" mainnet, "b" testnet.
pub fn l1_signing_digest(connection_id: [u8; 32], is_mainnet: bool) -> [u8; 32] {
    static DOMAIN: OnceLock<[u8; 32]> = OnceLock::new();
    static AGENT_TYPE: OnceLock<[u8; 32]> = OnceLock::new();
    let dom = DOMAIN.get_or_init(|| domain_separator("Exchange", 1337));
    let ty = AGENT_TYPE.get_or_init(|| keccak256(b"Agent(string source,bytes32 connectionId)"));
    let source = keccak256(if is_mainnet { b"a" } else { b"b" });
    let sh = keccak256_concat(&[ty, &source, &connection_id]);
    eip712_digest(dom, &sh)
}

/// Sign an L1 action JSON (msgpack-hashed with key order preserved).
/// Panics if the action is not msgpack-encodable (numbers beyond 64 bits).
pub fn sign_l1_action(
    w: &Wallet,
    action: &Value,
    nonce: u64,
    vault: Option<Address>,
    expires_after: Option<u64>,
    is_mainnet: bool,
) -> Sig65 {
    let bytes = json_to_msgpack(action).expect("sign_l1_action: action not msgpack-encodable");
    let cid = l1_action_hash(&bytes, nonce, vault, expires_after);
    w.sign_digest(&l1_signing_digest(cid, is_mainnet))
}

// ------------------------------------------------------------ user-signed

/// EIP-712 field types used by user-signed actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldType {
    String,
    Address,
    Uint64,
    Bool,
    Bytes32,
}

impl FieldType {
    fn name(self) -> &'static str {
        match self {
            FieldType::String => "string",
            FieldType::Address => "address",
            FieldType::Uint64 => "uint64",
            FieldType::Bool => "bool",
            FieldType::Bytes32 => "bytes32",
        }
    }
}

use FieldType as F;

/// (primary type, fields) for a user-signed action `type`, matching
/// `hyperliquid/utils/signing.py` and the TS SDK `*Types` constants.
pub fn user_signed_types(action_type: &str) -> Option<(&'static str, &'static [(&'static str, FieldType)])> {
    const CHAIN: (&str, FieldType) = ("hyperliquidChain", F::String);
    Some(match action_type {
        "usdSend" => (
            "HyperliquidTransaction:UsdSend",
            &[CHAIN, ("destination", F::String), ("amount", F::String), ("time", F::Uint64)],
        ),
        "spotSend" => (
            "HyperliquidTransaction:SpotSend",
            &[CHAIN, ("destination", F::String), ("token", F::String), ("amount", F::String), ("time", F::Uint64)],
        ),
        "withdraw3" => (
            "HyperliquidTransaction:Withdraw",
            &[CHAIN, ("destination", F::String), ("amount", F::String), ("time", F::Uint64)],
        ),
        "usdClassTransfer" => (
            "HyperliquidTransaction:UsdClassTransfer",
            &[CHAIN, ("amount", F::String), ("toPerp", F::Bool), ("nonce", F::Uint64)],
        ),
        "approveAgent" => (
            "HyperliquidTransaction:ApproveAgent",
            &[CHAIN, ("agentAddress", F::Address), ("agentName", F::String), ("nonce", F::Uint64)],
        ),
        "approveBuilderFee" => (
            "HyperliquidTransaction:ApproveBuilderFee",
            &[CHAIN, ("maxFeeRate", F::String), ("builder", F::Address), ("nonce", F::Uint64)],
        ),
        "tokenDelegate" => (
            "HyperliquidTransaction:TokenDelegate",
            &[CHAIN, ("validator", F::Address), ("wei", F::Uint64), ("isUndelegate", F::Bool), ("nonce", F::Uint64)],
        ),
        "sendAsset" => (
            "HyperliquidTransaction:SendAsset",
            &[
                CHAIN,
                ("destination", F::String),
                ("sourceDex", F::String),
                ("destinationDex", F::String),
                ("token", F::String),
                ("amount", F::String),
                ("fromSubAccount", F::String),
                ("nonce", F::Uint64),
            ],
        ),
        "userDexAbstraction" => (
            "HyperliquidTransaction:UserDexAbstraction",
            &[CHAIN, ("user", F::Address), ("enabled", F::Bool), ("nonce", F::Uint64)],
        ),
        "convertToMultiSigUser" => (
            "HyperliquidTransaction:ConvertToMultiSigUser",
            &[CHAIN, ("signers", F::String), ("nonce", F::Uint64)],
        ),
        _ => return None,
    })
}

/// Name of the nonce-carrying field ("time" or "nonce") of a user-signed type.
pub fn user_signed_nonce_field(action_type: &str) -> Option<&'static str> {
    let (_, fields) = user_signed_types(action_type)?;
    fields.iter().map(|(n, _)| *n).find(|n| *n == "time" || *n == "nonce")
}

fn parse_chain_id(s: &str) -> Result<u64, String> {
    let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).ok_or("signatureChainId must be 0x-hex")?;
    if h.is_empty() || h.len() > 16 {
        return Err(format!("invalid signatureChainId: {s}"));
    }
    u64::from_str_radix(h, 16).map_err(|_| format!("invalid signatureChainId: {s}"))
}

fn encode_field(name: &str, ty: FieldType, v: Option<&Value>, out: &mut Vec<u8>) -> Result<(), String> {
    // approveAgent.agentName: absent/null is signed as "" by both SDKs.
    let v = match v {
        Some(Value::Null) | None if ty == F::String && name == "agentName" => {
            out.extend_from_slice(&keccak256(b""));
            return Ok(());
        }
        Some(v) => v,
        None => return Err(format!("missing field `{name}`")),
    };
    match ty {
        F::String => {
            let s = v.as_str().ok_or_else(|| format!("field `{name}` must be a string"))?;
            out.extend_from_slice(&keccak256(s.as_bytes()));
        }
        F::Address => {
            let s = v.as_str().ok_or_else(|| format!("field `{name}` must be an address string"))?;
            let a = parse_address(s)?;
            out.extend_from_slice(&[0u8; 12]);
            out.extend_from_slice(&a.0);
        }
        F::Uint64 => {
            let n = v.as_u64().filter(|_| v.is_u64()).ok_or_else(|| format!("field `{name}` must be a uint64"))?;
            if v.as_number().map(|x| x.to_string().contains(['.', 'e', 'E'])).unwrap_or(true) {
                return Err(format!("field `{name}` must be an integer"));
            }
            out.extend_from_slice(&u256_be(n));
        }
        F::Bool => {
            let b = v.as_bool().ok_or_else(|| format!("field `{name}` must be a bool"))?;
            out.extend_from_slice(&u256_be(b as u64));
        }
        F::Bytes32 => {
            let s = v.as_str().ok_or_else(|| format!("field `{name}` must be a hex string"))?;
            let h = s.strip_prefix("0x").unwrap_or(s);
            let mut b = [0u8; 32];
            hex::decode_to_slice(h, &mut b).map_err(|_| format!("field `{name}` must be bytes32 hex"))?;
            out.extend_from_slice(&b);
        }
    }
    Ok(())
}

/// EIP-712 digest of a user-signed action: domain
/// `HyperliquidSignTransaction / 1 / int(signatureChainId) / 0x0`, primary
/// type `HyperliquidTransaction:<Name>` chosen from `action["type"]`.
/// Extra keys (`type`, `signatureChainId`, ...) are ignored as in the SDKs.
pub fn user_signed_digest(action: &Value) -> Result<[u8; 32], String> {
    let obj = action.as_object().ok_or("action must be an object")?;
    let t = obj.get("type").and_then(Value::as_str).ok_or("action.type missing")?;
    let (primary, fields) = user_signed_types(t).ok_or_else(|| format!("{t} is not a user-signed action"))?;
    let chain = obj.get("signatureChainId").and_then(Value::as_str).ok_or("signatureChainId missing")?;
    let chain_id = parse_chain_id(chain)?;

    let mut type_str = String::with_capacity(128);
    type_str.push_str(primary);
    type_str.push('(');
    for (i, (n, ty)) in fields.iter().enumerate() {
        if i > 0 {
            type_str.push(',');
        }
        type_str.push_str(ty.name());
        type_str.push(' ');
        type_str.push_str(n);
    }
    type_str.push(')');

    let mut enc = Vec::with_capacity(32 * (fields.len() + 1));
    enc.extend_from_slice(&keccak256(type_str.as_bytes()));
    for (n, ty) in fields {
        encode_field(n, *ty, obj.get(*n), &mut enc)?;
    }
    let sh = keccak256(&enc);
    Ok(eip712_digest(&domain_separator("HyperliquidSignTransaction", chain_id), &sh))
}

/// Sign a user-signed action like the Python SDK: sets
/// `signatureChainId = "0x66eee"` and `hyperliquidChain` ("Mainnet"/"Testnet")
/// on the action (appended if absent, replaced in place otherwise), then
/// signs the EIP-712 digest. Panics if required fields are missing.
pub fn sign_user_signed_action(w: &Wallet, action: &mut Value, is_mainnet: bool) -> Sig65 {
    let obj = action.as_object_mut().expect("sign_user_signed_action: action must be an object");
    obj.insert("signatureChainId".into(), Value::String(DEFAULT_SIGNATURE_CHAIN_ID.into()));
    obj.insert("hyperliquidChain".into(), Value::String(hyperliquid_chain(is_mainnet).into()));
    let d = user_signed_digest(action).expect("sign_user_signed_action: invalid user-signed action");
    w.sign_digest(&d)
}

/// "Mainnet" / "Testnet".
pub fn hyperliquid_chain(is_mainnet: bool) -> &'static str {
    if is_mainnet {
        "Mainnet"
    } else {
        "Testnet"
    }
}

// ------------------------------------------------------------ /exchange body

/// HL signature JSON `{"r":"0x..","s":"0x..","v":27}` (32-byte padded hex).
pub fn sig_to_json(sig: &Sig65) -> Value {
    let mut m = Map::new();
    m.insert("r".into(), Value::String(format!("0x{}", hex::encode(sig.r))));
    m.insert("s".into(), Value::String(format!("0x{}", hex::encode(sig.s))));
    m.insert("v".into(), Value::from(sig.v));
    Value::Object(m)
}

fn sig_word(v: Option<&Value>, k: &str) -> Result<[u8; 32], String> {
    let s = v.and_then(Value::as_str).ok_or_else(|| format!("signature.{k} must be a hex string"))?;
    let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if h.is_empty() || h.len() > 64 || !h.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("invalid signature.{k}"));
    }
    let padded = format!("{:0>64}", h);
    let mut o = [0u8; 32];
    hex::decode_to_slice(&padded, &mut o).map_err(|_| format!("invalid signature.{k}"))?;
    Ok(o)
}

/// Parse an HL signature object; `r`/`s` may lack leading zeros, `v` may be
/// 27/28 or 0/1 (normalized to 27/28).
pub fn parse_signature(v: &Value) -> Result<Sig65, String> {
    let o = v.as_object().ok_or("signature must be an object")?;
    let r = sig_word(o.get("r"), "r")?;
    let s = sig_word(o.get("s"), "s")?;
    let vv = o.get("v").and_then(Value::as_u64).ok_or("signature.v must be an integer")?;
    let v = match vv {
        27 | 28 => vv as u8,
        0 | 1 => vv as u8 + 27,
        _ => return Err(format!("invalid signature.v: {vv}")),
    };
    Ok(Sig65 { r, s, v })
}

fn opt_u64(body: &Map<String, Value>, k: &str) -> Result<Option<u64>, String> {
    match body.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| format!("{k} must be an unsigned integer")),
    }
}

/// Parse a `/exchange` body `{action, nonce, signature{r,s,v}, vaultAddress?, expiresAfter?}`
/// into a [`UserTx`]. The action is msgpack-encoded with its key order kept
/// (the bytes the L1 signature hashes). The action is not decoded here.
pub fn parse_exchange_request(body: &Value) -> Result<UserTx, String> {
    let b = body.as_object().ok_or("request body must be a JSON object")?;
    let action = b.get("action").ok_or("missing action")?;
    let ao = action.as_object().ok_or("action must be an object")?;
    if !ao.get("type").map(Value::is_string).unwrap_or(false) {
        return Err("action.type must be a string".into());
    }
    let nonce = b.get("nonce").and_then(Value::as_u64).ok_or("nonce must be an unsigned integer")?;
    let signature = parse_signature(b.get("signature").ok_or("missing signature")?)?;
    let vault_address = match b.get("vaultAddress") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(parse_address(s)?),
        Some(_) => return Err("vaultAddress must be a string".into()),
    };
    let expires_after = opt_u64(b, "expiresAfter")?;
    Ok(UserTx { action_bytes: json_to_msgpack(action)?, nonce, signature, vault_address, expires_after })
}

/// Build a `/exchange` body in Python SDK key order
/// (`action, nonce, signature, vaultAddress?, expiresAfter?`); optional keys
/// are omitted when `None` (as the TS SDK does).
pub fn build_exchange_request(
    action: Value,
    nonce: u64,
    sig: Sig65,
    vault: Option<Address>,
    expires_after: Option<u64>,
) -> Value {
    let mut m = Map::new();
    m.insert("action".into(), action);
    m.insert("nonce".into(), Value::from(nonce));
    m.insert("signature".into(), sig_to_json(&sig));
    if let Some(v) = vault {
        m.insert("vaultAddress".into(), Value::String(v.to_hex()));
    }
    if let Some(e) = expires_after {
        m.insert("expiresAfter".into(), Value::from(e));
    }
    Value::Object(m)
}

/// Convenience: sign an L1 action and build a ready-to-post body (and
/// optionally the `UserTx` via [`parse_exchange_request`]).
pub fn signed_l1_request(
    w: &Wallet,
    action: Value,
    nonce: u64,
    vault: Option<Address>,
    expires_after: Option<u64>,
    is_mainnet: bool,
) -> Value {
    let sig = sign_l1_action(w, &action, nonce, vault, expires_after, is_mainnet);
    build_exchange_request(action, nonce, sig, vault, expires_after)
}

/// Convenience: sign a user-signed action (Python flavour) and build the body.
pub fn signed_user_request(w: &Wallet, mut action: Value, is_mainnet: bool) -> Value {
    let sig = sign_user_signed_action(w, &mut action, is_mainnet);
    let t = action["type"].as_str().unwrap_or_default().to_string();
    let nf = user_signed_nonce_field(&t).expect("user-signed type");
    let nonce = action[nf].as_u64().expect("nonce field");
    build_exchange_request(action, nonce, sig, None, None)
}

/// Build a `UserTx` directly (skips JSON body).
pub fn user_tx_from_parts(
    action: &Value,
    nonce: u64,
    signature: Sig65,
    vault: Option<Address>,
    expires_after: Option<u64>,
) -> Result<UserTx, String> {
    Ok(UserTx { action_bytes: json_to_msgpack(action)?, nonce, signature, vault_address: vault, expires_after })
}

// ------------------------------------------------------------ verifier

/// Recover the signer of a user tx (L1 or user-signed scheme picked from the
/// action `type`). For user-signed actions also enforces:
/// `hyperliquidChain` matches `is_mainnet`, `expiresAfter` is absent, and the
/// tx nonce equals the action's `time`/`nonce` field.
pub fn recover_signer(tx: &UserTx, is_mainnet: bool) -> Result<Address, String> {
    let t = crate::decode::action_type(&tx.action_bytes)?;
    recover_signer_typed(tx, &t, is_mainnet)
}

fn recover_signer_typed(tx: &UserTx, t: &str, is_mainnet: bool) -> Result<Address, String> {
    if user_signed_types(t).is_some() {
        let action = crate::msgpack::msgpack_to_json(&tx.action_bytes)?;
        let chain = action.get("hyperliquidChain").and_then(Value::as_str).ok_or("hyperliquidChain missing")?;
        if chain != hyperliquid_chain(is_mainnet) {
            return Err(format!("hyperliquidChain {chain} does not match this chain"));
        }
        if tx.expires_after.is_some() {
            return Err("expiresAfter is not supported for user-signed actions".into());
        }
        let nf = user_signed_nonce_field(t).ok_or("no nonce field")?;
        let an = action.get(nf).and_then(Value::as_u64).ok_or_else(|| format!("{nf} missing"))?;
        if an != tx.nonce {
            return Err(format!("nonce {} does not match action {nf} {an}", tx.nonce));
        }
        recover(&user_signed_digest(&action)?, &tx.signature)
    } else {
        let cid = l1_action_hash(&tx.action_bytes, tx.nonce, tx.vault_address, tx.expires_after);
        recover(&l1_signing_digest(cid, is_mainnet), &tx.signature)
    }
}

fn action_type_name(a: &Action) -> &'static str {
    match a {
        Action::Order { .. } => "order",
        Action::Cancel { .. } => "cancel",
        Action::CancelByCloid { .. } => "cancelByCloid",
        Action::Modify { .. } => "modify",
        Action::BatchModify { .. } => "batchModify",
        Action::ScheduleCancel { .. } => "scheduleCancel",
        Action::UpdateLeverage { .. } => "updateLeverage",
        Action::UpdateIsolatedMargin { .. } => "updateIsolatedMargin",
        Action::UsdSend { .. } => "usdSend",
        Action::SpotSend { .. } => "spotSend",
        Action::UsdClassTransfer { .. } => "usdClassTransfer",
        Action::Withdraw3 { .. } => "withdraw3",
        Action::ApproveAgent { .. } => "approveAgent",
        Action::SetReferrer { .. } => "setReferrer",
        Action::Noop => "noop",
    }
}

/// True if this typed action is user-signed (EIP-712 HyperliquidTransaction).
pub fn is_user_signed(a: &Action) -> bool {
    matches!(
        a,
        Action::UsdSend { .. }
            | Action::SpotSend { .. }
            | Action::UsdClassTransfer { .. }
            | Action::Withdraw3 { .. }
            | Action::ApproveAgent { .. }
    )
}

/// HL signature verifier for the state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HlVerifier {
    pub is_mainnet: bool,
}

impl HlVerifier {
    pub fn new(is_mainnet: bool) -> Self {
        HlVerifier { is_mainnet }
    }
}

impl TxVerifier for HlVerifier {
    fn decode(&self, tx: &UserTx) -> Result<Action, String> {
        decode_action(&tx.action_bytes)
    }

    fn recover_signer(&self, tx: &UserTx, action: &Action) -> Result<Address, String> {
        recover_signer_typed(tx, action_type_name(action), self.is_mainnet)
    }

    fn recover_validator(&self, tx: &ValidatorTx, chain_id: u64) -> Result<Address, String> {
        recover_validator_tx(tx, chain_id)
    }
}
