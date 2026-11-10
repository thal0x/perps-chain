//! Minimal secp256k1 helpers (Ethereum-style recovery / signing).

use interfaces::*;
use secp256k1::ecdsa::{RecoverableSignature, RecoveryId};
use secp256k1::{Message, PublicKey, SecretKey, SECP256K1};
use sha3::{Digest, Keccak256};

pub fn keccak(b: &[u8]) -> [u8; 32] {
    Keccak256::digest(b).into()
}

pub fn address_of(pk: &PublicKey) -> Address {
    let ser = pk.serialize_uncompressed();
    let h = keccak(&ser[1..]);
    let mut a = [0u8; 20];
    a.copy_from_slice(&h[12..]);
    Address(a)
}

/// Recover the signer of `digest`; accepts v in {0,1,27,28}.
pub fn ecrecover(digest: &[u8; 32], sig: &Sig65) -> Option<Address> {
    let v = match sig.v {
        0 | 27 => 0,
        1 | 28 => 1,
        _ => return None,
    };
    let id = RecoveryId::try_from(v as i32).ok()?;
    let mut rs = [0u8; 64];
    rs[..32].copy_from_slice(&sig.r);
    rs[32..].copy_from_slice(&sig.s);
    let rsig = RecoverableSignature::from_compact(&rs, id).ok()?;
    let pk = SECP256K1.recover_ecdsa(&Message::from_digest(*digest), &rsig).ok()?;
    Some(address_of(&pk))
}

/// Sign a digest (v = 27/28). Used by tests and tooling.
pub fn sign_digest(sk: &SecretKey, digest: &[u8; 32]) -> Sig65 {
    let rs = SECP256K1.sign_ecdsa_recoverable(&Message::from_digest(*digest), sk);
    let (id, bytes) = rs.serialize_compact();
    let mut r = [0u8; 32];
    let mut s = [0u8; 32];
    r.copy_from_slice(&bytes[..32]);
    s.copy_from_slice(&bytes[32..]);
    Sig65 { r, s, v: 27 + i32::from(id) as u8 }
}

pub fn address_of_secret(sk: &SecretKey) -> Address {
    address_of(&PublicKey::from_secret_key(SECP256K1, sk))
}
