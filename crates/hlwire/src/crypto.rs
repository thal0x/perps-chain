//! secp256k1 / keccak primitives: wallets, signing, recovery, validator and
//! consensus digests.

use borsh::BorshSerialize;
use interfaces::{Address, Sig65, ValidatorAction, ValidatorTx};
use secp256k1::ecdsa::{RecoverableSignature, RecoveryId};
use secp256k1::{Message, PublicKey, SecretKey, SECP256K1};
use sha3::{Digest, Keccak256};

/// keccak256 of `data`.
pub fn keccak256(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}

/// keccak256 of the concatenation of `parts`.
pub fn keccak256_concat(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Keccak256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// Ethereum address of a public key: last 20 bytes of keccak(uncompressed[1..]).
pub fn pubkey_to_address(pk: &PublicKey) -> Address {
    let ser = pk.serialize_uncompressed();
    let h = keccak256(&ser[1..]);
    let mut a = [0u8; 20];
    a.copy_from_slice(&h[12..]);
    Address(a)
}

/// Normalize an Ethereum `v` (27/28 or 0/1) to a recovery id 0/1.
pub fn recovery_id(v: u8) -> Result<u8, String> {
    match v {
        27 | 28 => Ok(v - 27),
        0 | 1 => Ok(v),
        _ => Err(format!("invalid signature v: {v}")),
    }
}

/// Recover the signer address of a 32-byte digest. Accepts `v` in {0,1,27,28}.
pub fn recover(digest: &[u8; 32], sig: &Sig65) -> Result<Address, String> {
    let rid = recovery_id(sig.v)?;
    if sig.r == [0u8; 32] || sig.s == [0u8; 32] {
        return Err("invalid signature: zero r or s".into());
    }
    let mut compact = [0u8; 64];
    compact[..32].copy_from_slice(&sig.r);
    compact[32..].copy_from_slice(&sig.s);
    let rid = RecoveryId::try_from(rid as i32).map_err(|e| e.to_string())?;
    let rs = RecoverableSignature::from_compact(&compact, rid)
        .map_err(|e| format!("invalid signature: {e}"))?;
    let pk = SECP256K1
        .recover_ecdsa(&Message::from_digest(*digest), &rs)
        .map_err(|e| format!("signature recovery failed: {e}"))?;
    Ok(pubkey_to_address(&pk))
}

/// A secp256k1 private key with its Ethereum address.
#[derive(Clone)]
pub struct Wallet {
    sk: SecretKey,
    address: Address,
}

impl std::fmt::Debug for Wallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Wallet({})", self.address)
    }
}

impl PartialEq for Wallet {
    fn eq(&self, o: &Self) -> bool {
        self.sk.secret_bytes() == o.sk.secret_bytes()
    }
}
impl Eq for Wallet {}

impl Wallet {
    /// From a 32-byte secret key.
    pub fn from_bytes(b: &[u8; 32]) -> Result<Self, String> {
        let sk = SecretKey::from_byte_array(b).map_err(|e| format!("invalid secret key: {e}"))?;
        let address = pubkey_to_address(&PublicKey::from_secret_key(SECP256K1, &sk));
        Ok(Wallet { sk, address })
    }

    /// From a hex secret (with or without `0x`).
    pub fn from_secret_hex(s: &str) -> Result<Self, String> {
        let h = s.trim();
        let h = h
            .strip_prefix("0x")
            .or_else(|| h.strip_prefix("0X"))
            .unwrap_or(h);
        let v = hex::decode(h).map_err(|_| "invalid secret key hex".to_string())?;
        let b: [u8; 32] = v
            .try_into()
            .map_err(|_| "secret key must be 32 bytes".to_string())?;
        Self::from_bytes(&b)
    }

    /// Random key from the given RNG (pass a seeded `ChaCha20Rng` for
    /// reproducible keys).
    pub fn random<R: rand::RngCore + ?Sized>(rng: &mut R) -> Self {
        loop {
            let mut b = [0u8; 32];
            rng.fill_bytes(&mut b);
            if let Ok(w) = Self::from_bytes(&b) {
                return w;
            }
        }
    }

    pub fn address(&self) -> Address {
        self.address
    }

    pub fn secret_bytes(&self) -> [u8; 32] {
        self.sk.secret_bytes()
    }

    /// `0x`-prefixed lowercase hex secret (what the SDKs take as a private key).
    pub fn secret_hex(&self) -> String {
        format!("0x{}", hex::encode(self.sk.secret_bytes()))
    }

    /// Sign a 32-byte digest (RFC 6979 deterministic nonce, low-s), `v` = 27/28.
    /// Produces the same signature as eth_account / viem for the same digest.
    pub fn sign_digest(&self, digest: &[u8; 32]) -> Sig65 {
        let rs = SECP256K1.sign_ecdsa_recoverable(&Message::from_digest(*digest), &self.sk);
        let (rid, compact) = rs.serialize_compact();
        let mut r = [0u8; 32];
        let mut s = [0u8; 32];
        r.copy_from_slice(&compact[..32]);
        s.copy_from_slice(&compact[32..]);
        Sig65 {
            r,
            s,
            v: 27 + i32::from(rid) as u8,
        }
    }
}

// ------------------------------------------------------------ validator txs

/// keccak256(borsh((chain_id, validator, nonce, action))).
pub fn validator_tx_digest(
    chain_id: u64,
    validator: &Address,
    nonce: u64,
    action: &ValidatorAction,
) -> [u8; 32] {
    let mut buf = Vec::with_capacity(96);
    // Serializing the fields in order is byte-identical to borsh of the tuple.
    chain_id.serialize(&mut buf).expect("vec write");
    validator.serialize(&mut buf).expect("vec write");
    nonce.serialize(&mut buf).expect("vec write");
    action.serialize(&mut buf).expect("vec write");
    keccak256(&buf)
}

/// Build and sign a validator transaction with the validator's key.
pub fn sign_validator_tx(
    w: &Wallet,
    chain_id: u64,
    nonce: u64,
    action: ValidatorAction,
) -> ValidatorTx {
    let validator = w.address();
    let d = validator_tx_digest(chain_id, &validator, nonce, &action);
    ValidatorTx {
        validator,
        nonce,
        action,
        signature: w.sign_digest(&d),
    }
}

/// Verify a validator tx; returns the signer (must equal `tx.validator`).
pub fn recover_validator_tx(tx: &ValidatorTx, chain_id: u64) -> Result<Address, String> {
    let d = validator_tx_digest(chain_id, &tx.validator, tx.nonce, &tx.action);
    let a = recover(&d, &tx.signature)?;
    if a != tx.validator {
        return Err(format!(
            "validator tx signed by {a}, expected {}",
            tx.validator
        ));
    }
    Ok(a)
}

/// keccak256(domain || payload_borsh). Used for consensus votes, timeouts and
/// the p2p handshake (each with its own domain tag).
pub fn consensus_digest(domain: &[u8], payload_borsh: &[u8]) -> [u8; 32] {
    keccak256_concat(&[domain, payload_borsh])
}

/// Verify `sig` over `digest` was produced by `signer`.
pub fn verify(digest: &[u8; 32], sig: &Sig65, signer: &Address) -> bool {
    matches!(recover(digest, sig), Ok(a) if a == *signer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn known_address() {
        // Well-known test key (eth_account docs).
        let w = Wallet::from_secret_hex(
            "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318",
        )
        .unwrap();
        assert_eq!(
            w.address().to_hex(),
            "0x2c7536e3605d9c16a7a3d7b1898e529396a65c23"
        );
        assert_eq!(Wallet::from_secret_hex(&w.secret_hex()).unwrap(), w);
    }

    #[test]
    fn sign_recover_roundtrip_and_v_forms() {
        let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(7);
        for i in 0..20u8 {
            let w = Wallet::random(&mut rng);
            let d = keccak256(&[i]);
            let mut sig = w.sign_digest(&d);
            assert!(sig.v == 27 || sig.v == 28);
            assert_eq!(recover(&d, &sig).unwrap(), w.address());
            sig.v -= 27;
            assert_eq!(recover(&d, &sig).unwrap(), w.address());
            sig.v = 29;
            assert!(recover(&d, &sig).is_err());
        }
        let w2 = Wallet::random(&mut rand_chacha::ChaCha20Rng::seed_from_u64(7));
        let w3 = Wallet::random(&mut rand_chacha::ChaCha20Rng::seed_from_u64(7));
        assert_eq!(w2, w3);
    }

    #[test]
    fn rejects_bad_sigs() {
        let d = [1u8; 32];
        assert!(recover(
            &d,
            &Sig65 {
                r: [0; 32],
                s: [1; 32],
                v: 27
            }
        )
        .is_err());
        assert!(recover(
            &d,
            &Sig65 {
                r: [0xff; 32],
                s: [0xff; 32],
                v: 27
            }
        )
        .is_err());
        assert!(Wallet::from_bytes(&[0u8; 32]).is_err());
        assert!(Wallet::from_secret_hex("0x1234").is_err());
    }

    #[test]
    fn validator_tx_sign_verify() {
        let w = Wallet::random(&mut rand_chacha::ChaCha20Rng::seed_from_u64(1));
        let tx = sign_validator_tx(
            &w,
            42,
            5,
            ValidatorAction::WithdrawalFinalized { withdrawal_id: 3 },
        );
        assert_eq!(recover_validator_tx(&tx, 42).unwrap(), w.address());
        assert!(recover_validator_tx(&tx, 43).is_err());
        let mut t2 = tx.clone();
        t2.nonce = 6;
        assert!(recover_validator_tx(&t2, 42).is_err());
        // digest == keccak(borsh(tuple))
        let tuple = (
            42u64,
            w.address(),
            5u64,
            ValidatorAction::WithdrawalFinalized { withdrawal_id: 3 },
        );
        assert_eq!(
            validator_tx_digest(42, &w.address(), 5, &tuple.3),
            keccak256(&borsh::to_vec(&tuple).unwrap())
        );
        assert_eq!(consensus_digest(b"ab", b"cd"), keccak256(b"abcd"));
    }
}
