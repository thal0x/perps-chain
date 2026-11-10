//! Unsigned actions produced by the agents, and (parallel, order-preserving)
//! signing into block txs and/or `/exchange` JSON bodies.

use crate::genesis::SimKeys;
use hlwire::Wallet;
use interfaces::*;
use rayon::prelude::*;

/// Who signs a user action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signer {
    /// The account's own key.
    Account(usize),
    /// The account's approved agent wallet (L1 actions only).
    Agent(usize),
    /// A key unknown to the chain ("does not exist" error path).
    Stranger(u64),
}

/// Intentional invalidity applied to a user tx (error paths).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Corruption {
    None,
    /// Flip a byte of `s` (recovers to a random address or fails).
    BadSignature,
}

#[derive(Clone, Debug)]
pub enum Unsigned {
    User {
        signer: Signer,
        action: Action,
        nonce: u64,
        expires_after: Option<u64>,
        corruption: Corruption,
        /// True if the generator deliberately broke the tx in any way (it
        /// is then never primed into the embedded app's signature cache).
        invalid: bool,
    },
    Validator {
        validator: usize,
        nonce: u64,
        action: ValidatorAction,
    },
}

impl Unsigned {
    pub fn order_actions(&self) -> u64 {
        match self {
            Unsigned::User { action, .. } => action.order_action_count() as u64,
            Unsigned::Validator { .. } => 0,
        }
    }
}

/// Signed output of one unsigned action.
#[derive(Clone, Debug)]
pub struct Signed {
    pub tx: Tx,
    /// `/exchange` JSON body (only when requested, user txs only).
    pub body: Option<Vec<u8>>,
    /// (decoded action, signing address) when the tx is known-good.
    pub known: Option<(Action, Address)>,
    pub order_actions: u64,
}

pub struct SignCtx<'a> {
    pub keys: &'a SimKeys,
    pub chain_id: u64,
    pub is_mainnet: bool,
    pub want_body: bool,
}

fn wallet_for(keys: &SimKeys, s: Signer) -> Wallet {
    match s {
        Signer::Account(i) => keys.accounts[i].clone(),
        Signer::Agent(i) => keys.agents[i].clone(),
        Signer::Stranger(k) => keys.stranger(k),
    }
}

fn wallet_ref(keys: &SimKeys, s: Signer) -> std::borrow::Cow<'_, Wallet> {
    match s {
        Signer::Account(i) => std::borrow::Cow::Borrowed(&keys.accounts[i]),
        Signer::Agent(i) => std::borrow::Cow::Borrowed(&keys.agents[i]),
        Signer::Stranger(_) => std::borrow::Cow::Owned(wallet_for(keys, s)),
    }
}

/// Sign one unsigned action.
pub fn sign_one(cx: &SignCtx<'_>, u: &Unsigned) -> Signed {
    match u {
        Unsigned::Validator { validator, nonce, action } => {
            let w = &cx.keys.validators[*validator];
            let tx = hlwire::sign_validator_tx(w, cx.chain_id, *nonce, action.clone());
            Signed { tx: Tx::Validator(tx), body: None, known: None, order_actions: 0 }
        }
        Unsigned::User { signer, action, nonce, expires_after, corruption, invalid } => {
            let w = wallet_ref(cx.keys, *signer);
            let mut json = hlwire::action_to_json(action);
            let mut sig = if hlwire::is_user_signed(action) {
                // Python flavour: sets signatureChainId / hyperliquidChain.
                hlwire::sign_user_signed_action(&w, &mut json, cx.is_mainnet)
            } else {
                let bytes = hlwire::json_to_msgpack(&json).expect("msgpack");
                let cid = hlwire::l1_action_hash(&bytes, *nonce, None, *expires_after);
                w.sign_digest(&hlwire::l1_signing_digest(cid, cx.is_mainnet))
            };
            if *corruption == Corruption::BadSignature {
                sig.s[7] ^= 0x5a;
                sig.r[3] ^= 0x11;
            }
            let utx = hlwire::user_tx_from_parts(&json, *nonce, sig, None, *expires_after).expect("msgpack");
            let known = if *invalid || *corruption != Corruption::None {
                None
            } else {
                hlwire::decode_action(&utx.action_bytes).ok().map(|a| (a, w.address()))
            };
            let body = if cx.want_body {
                let b = hlwire::build_exchange_request(json, *nonce, sig, None, *expires_after);
                Some(serde_json::to_vec(&b).expect("json"))
            } else {
                None
            };
            Signed { tx: Tx::User(utx), body, known, order_actions: action.order_action_count() as u64 }
        }
    }
}

/// Sign in parallel (rayon), preserving input order: the result is
/// independent of the thread count.
pub fn sign_all(cx: &SignCtx<'_>, us: &[Unsigned]) -> Vec<Signed> {
    us.par_iter().with_min_len(8).map(|u| sign_one(cx, u)).collect()
}

/// Order actions of an encoded user action (0 if undecodable).
pub fn order_action_count_bytes(action_bytes: &[u8]) -> u64 {
    hlwire::decode_action(action_bytes).map(|a| a.order_action_count() as u64).unwrap_or(0)
}
