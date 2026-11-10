//! EIP-712 digests identical to `bridge/src/Bridge.sol`, plus secp256k1
//! sign/recover helpers in Ethereum form.
//!
//! Domain: `EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)`
//! with name `"HlbftBridge"`, version `"1"`.
//! Withdrawal: `Withdrawal(uint64 id,address destination,uint256 amount)`; the
//! withdrawal id doubles as the nonce (the contract's `usedWithdrawal[id]`).
//! Validator set update: `ValidatorSetUpdate(bytes32 validatorsHash,bytes32 powersHash,uint64 epoch)`
//! where `validatorsHash = keccak256(abi.encodePacked(address[]))` (each
//! element left-padded to 32 bytes) and `powersHash = keccak256(abi.encodePacked(uint256[]))`.

use crate::abi::{keccak256, word_address, word_u128, word_u64};
use interfaces::{Address, BridgeParams, PendingWithdrawal, Sig65};
use secp256k1::ecdsa::{RecoverableSignature, RecoveryId};
use secp256k1::{Message, PublicKey, SecretKey, SECP256K1};

pub const DOMAIN_NAME: &str = "HlbftBridge";
pub const DOMAIN_VERSION: &str = "1";
pub const DOMAIN_TYPE: &str = "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
pub const WITHDRAWAL_TYPE: &str = "Withdrawal(uint64 id,address destination,uint256 amount)";
pub const VALIDATOR_SET_UPDATE_TYPE: &str =
    "ValidatorSetUpdate(bytes32 validatorsHash,bytes32 powersHash,uint64 epoch)";

/// secp256k1 group order n / 2 (EIP-2 low-s bound).
pub const HALF_N: [u8; 32] = [
    0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x5D, 0x57,
    0x6E, 0x73, 0x57, 0xA4, 0x50, 0x1D, 0xDF, 0xE9, 0x2F, 0x46, 0x68, 0x1B, 0x20, 0xA0,
];

pub fn domain_separator(chain_id: u64, contract: &Address) -> [u8; 32] {
    let mut buf = Vec::with_capacity(160);
    buf.extend_from_slice(&keccak256(DOMAIN_TYPE.as_bytes()));
    buf.extend_from_slice(&keccak256(DOMAIN_NAME.as_bytes()));
    buf.extend_from_slice(&keccak256(DOMAIN_VERSION.as_bytes()));
    buf.extend_from_slice(&word_u64(chain_id));
    buf.extend_from_slice(&word_address(contract));
    keccak256(&buf)
}

fn typed_digest(domain: &[u8; 32], struct_hash: &[u8; 32]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(66);
    buf.extend_from_slice(&[0x19, 0x01]);
    buf.extend_from_slice(domain);
    buf.extend_from_slice(struct_hash);
    keccak256(&buf)
}

/// Digest of `Withdrawal{id, destination, amount}` with raw (6-decimal) amount.
pub fn withdrawal_digest_raw(id: u64, destination: &Address, amount: u128, chain_id: u64, contract: &Address) -> [u8; 32] {
    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(&keccak256(WITHDRAWAL_TYPE.as_bytes()));
    buf.extend_from_slice(&word_u64(id));
    buf.extend_from_slice(&word_address(destination));
    buf.extend_from_slice(&word_u128(amount));
    typed_digest(&domain_separator(chain_id, contract), &keccak256(&buf))
}

/// Withdrawal amount in USDC base units (6 decimals == `Usd` raw units).
/// Panics on a non-positive amount (the L1 never creates one).
pub fn withdrawal_amount_units(w: &PendingWithdrawal) -> u128 {
    let raw = w.amount.raw();
    assert!(raw > 0, "withdrawal {} has non-positive amount", w.id);
    raw as u128
}

/// The EIP-712 digest the validators sign and `Bridge.withdraw` verifies.
/// `amount` is the USDC amount transferred to `destination` (1e-6 units).
pub fn withdrawal_digest(w: &PendingWithdrawal, p: &BridgeParams) -> [u8; 32] {
    withdrawal_digest_raw(w.id, &w.destination, withdrawal_amount_units(w), p.eth_chain_id, &p.contract)
}

/// Digest of `ValidatorSetUpdate` for `Bridge.updateValidatorSet`.
pub fn validator_set_update_digest(
    validators: &[Address],
    powers: &[u128],
    epoch: u64,
    chain_id: u64,
    contract: &Address,
) -> [u8; 32] {
    let vbytes: Vec<u8> = validators.iter().flat_map(word_address).collect();
    let pbytes: Vec<u8> = powers.iter().flat_map(|p| word_u128(*p)).collect();
    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(&keccak256(VALIDATOR_SET_UPDATE_TYPE.as_bytes()));
    buf.extend_from_slice(&keccak256(&vbytes));
    buf.extend_from_slice(&keccak256(&pbytes));
    buf.extend_from_slice(&word_u64(epoch));
    typed_digest(&domain_separator(chain_id, contract), &keccak256(&buf))
}

/// Ethereum address of a public key.
pub fn pubkey_address(pk: &PublicKey) -> Address {
    let ser = pk.serialize_uncompressed();
    let h = keccak256(&ser[1..]);
    let mut a = [0u8; 20];
    a.copy_from_slice(&h[12..]);
    Address(a)
}

pub fn secret_address(sk: &SecretKey) -> Address {
    pubkey_address(&PublicKey::from_secret_key(SECP256K1, sk))
}

pub fn parse_secret_key(hex_str: &str) -> anyhow::Result<SecretKey> {
    let s = hex_str.trim().trim_start_matches("0x");
    let b = hex::decode(s)?;
    Ok(SecretKey::from_slice(&b)?)
}

/// Sign a 32-byte digest; returns low-s signature with v in {27, 28}.
pub fn sign_digest(sk: &SecretKey, digest: &[u8; 32]) -> Sig65 {
    let sig = SECP256K1.sign_ecdsa_recoverable(&Message::from_digest(*digest), sk);
    let (rid, bytes) = sig.serialize_compact();
    let mut r = [0u8; 32];
    let mut s = [0u8; 32];
    r.copy_from_slice(&bytes[..32]);
    s.copy_from_slice(&bytes[32..]);
    Sig65 { r, s, v: 27 + i32::from(rid) as u8 }
}

/// Normalise v to 27/28 (accepts 0/1). None for anything else.
pub fn normalize_v(v: u8) -> Option<u8> {
    match v {
        0 | 1 => Some(v + 27),
        27 | 28 => Some(v),
        _ => None,
    }
}

/// Recover the signer of `digest`, enforcing the same rules as Bridge.sol
/// (v in {0,1,27,28}, 0 < r, 0 < s <= n/2). Returns None if invalid.
pub fn recover_signer(digest: &[u8; 32], sig: &Sig65) -> Option<Address> {
    let v = normalize_v(sig.v)?;
    if sig.r == [0u8; 32] || sig.s == [0u8; 32] || sig.s > HALF_N {
        return None;
    }
    let rid = RecoveryId::try_from((v - 27) as i32).ok()?;
    let mut compact = [0u8; 64];
    compact[..32].copy_from_slice(&sig.r);
    compact[32..].copy_from_slice(&sig.s);
    let rs = RecoverableSignature::from_compact(&compact, rid).ok()?;
    let pk = SECP256K1.recover_ecdsa(&Message::from_digest(*digest), &rs).ok()?;
    Some(pubkey_address(&pk))
}

#[cfg(test)]
mod tests {
    use super::*;
    use interfaces::Usd;

    /// Anvil dev account 0.
    const KEY0: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    #[test]
    fn address_of_anvil_key() {
        let sk = parse_secret_key(KEY0).unwrap();
        assert_eq!(secret_address(&sk), Address::from_hex("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266").unwrap());
    }

    #[test]
    fn sign_recover_roundtrip_and_malleability() {
        let sk = parse_secret_key(KEY0).unwrap();
        let d = keccak256(b"hello");
        let sig = sign_digest(&sk, &d);
        assert!(sig.v == 27 || sig.v == 28);
        assert!(sig.s <= HALF_N);
        assert_eq!(recover_signer(&d, &sig), Some(secret_address(&sk)));
        // 0/1 form accepted
        let mut s01 = sig;
        s01.v -= 27;
        assert_eq!(recover_signer(&d, &s01), Some(secret_address(&sk)));
        // high-s rejected
        let mut hs = sig;
        hs.s = [0xff; 32];
        assert_eq!(recover_signer(&d, &hs), None);
        let mut bv = sig;
        bv.v = 29;
        assert_eq!(recover_signer(&d, &bv), None);
    }

    /// Pure test vector. Expected values were produced by Bridge.sol on Anvil
    /// (`cast call <bridge> "hashWithdrawal((uint64,address,uint256))" ...`)
    /// for the deterministic first deployment at 0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512,
    /// chain 31337 (see tests/anvil.rs for the live comparison).
    #[test]
    fn withdrawal_digest_vector() {
        let p = BridgeParams {
            eth_chain_id: 31337,
            contract: Address::from_hex("0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512").unwrap(),
            min_deposit: Usd::from_raw(5_000_000),
        };
        let w = PendingWithdrawal {
            id: 7,
            user: Address::ZERO,
            destination: Address::from_hex("0x70997970C51812dc3A010C7d01b50e0d17dc79C8").unwrap(),
            amount: Usd::from_raw(499_000_000),
            created_at: 0,
            signatures: vec![],
        };
        assert_eq!(
            hex::encode(domain_separator(p.eth_chain_id, &p.contract)),
            include_str!("../testdata/domain_separator.hex").trim()
        );
        assert_eq!(hex::encode(withdrawal_digest(&w, &p)), include_str!("../testdata/withdrawal_digest.hex").trim());
        let vs = [
            Address::from_hex("0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65").unwrap(),
            Address::from_hex("0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC").unwrap(),
        ];
        assert_eq!(
            hex::encode(validator_set_update_digest(&vs, &[1, 2], 5, p.eth_chain_id, &p.contract)),
            include_str!("../testdata/valset_digest.hex").trim()
        );
    }

    #[test]
    fn digest_binds_every_field() {
        let p = BridgeParams { eth_chain_id: 31337, contract: Address([1; 20]), min_deposit: Usd::from_raw(1) };
        let w = PendingWithdrawal {
            id: 1,
            user: Address([9; 20]),
            destination: Address([2; 20]),
            amount: Usd::from_raw(10),
            created_at: 5,
            signatures: vec![],
        };
        let base = withdrawal_digest(&w, &p);
        // user / created_at / signatures are not part of the signed message
        let mut w2 = w.clone();
        w2.user = Address([8; 20]);
        w2.created_at = 6;
        assert_eq!(withdrawal_digest(&w2, &p), base);
        let mut w3 = w.clone();
        w3.id = 2;
        assert_ne!(withdrawal_digest(&w3, &p), base);
        let mut w4 = w.clone();
        w4.destination = Address([3; 20]);
        assert_ne!(withdrawal_digest(&w4, &p), base);
        let mut w5 = w.clone();
        w5.amount = Usd::from_raw(11);
        assert_ne!(withdrawal_digest(&w5, &p), base);
        let mut p2 = p.clone();
        p2.eth_chain_id = 1;
        assert_ne!(withdrawal_digest(&w, &p2), base);
        let mut p3 = p.clone();
        p3.contract = Address([4; 20]);
        assert_ne!(withdrawal_digest(&w, &p3), base);
    }
}
