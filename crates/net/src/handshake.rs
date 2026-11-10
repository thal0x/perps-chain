//! Authenticated connection handshake.
//!
//! Both sides run the same symmetric protocol (no initiator/responder
//! asymmetry, so simultaneous writes cannot deadlock: each message is tiny):
//!
//! 1. send `Hello` = `MAGIC(8) || version u32 BE || chain_id u64 BE ||
//!    claimed address (20) || nonce (32)` (72 bytes);
//! 2. read the peer's `Hello`, check magic/version/chain id and that the
//!    claimed address is acceptable (validator, not ourselves, expected
//!    dialer/dialee);
//! 3. send `Sig65` (r || s || v, v = 27/28) over
//!    `keccak256("hlbft-handshake" || chain_id u64 BE || peer_nonce)`;
//! 4. read the peer's signature, recover the signer over our own nonce, and
//!    require it to equal the claimed address.
//!
//! The challenge binds the signature to a fresh nonce chosen by the verifier,
//! so a recorded handshake cannot be replayed. The transport is not encrypted
//! (out of scope: validators run on a private network in all test configs).

use interfaces::Address;
use secp256k1::ecdsa::{RecoverableSignature, RecoveryId};
use secp256k1::{Message, PublicKey, SecretKey, SECP256K1};
use sha3::{Digest, Keccak256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAGIC: &[u8; 8] = b"HLBFTP2P";
pub const VERSION: u32 = 1;
pub const DOMAIN: &[u8] = b"hlbft-handshake";
const HELLO_LEN: usize = 8 + 4 + 8 + 20 + 32;

#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad magic or version")]
    BadMagic,
    #[error("chain id mismatch: ours {ours}, theirs {theirs}")]
    ChainId { ours: u64, theirs: u64 },
    #[error("peer {0} not accepted")]
    NotAccepted(Address),
    #[error("bad signature from claimed peer {0}")]
    BadSignature(Address),
}

/// Our identity for handshakes.
#[derive(Clone)]
pub struct Identity {
    pub secret: SecretKey,
    pub address: Address,
    pub chain_id: u64,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("address", &self.address)
            .field("chain_id", &self.chain_id)
            .finish_non_exhaustive()
    }
}

impl Identity {
    pub fn new(secret_key: &[u8; 32], chain_id: u64) -> Result<Self, String> {
        let secret = SecretKey::from_byte_array(secret_key).map_err(|e| format!("bad secret key: {e}"))?;
        let address = address_of(&PublicKey::from_secret_key_global(&secret));
        Ok(Self { secret, address, chain_id })
    }
}

pub fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(data);
    h.finalize().into()
}

/// Ethereum address of a public key.
pub fn address_of(pk: &PublicKey) -> Address {
    let u = pk.serialize_uncompressed();
    let h = keccak256(&u[1..]);
    let mut a = [0u8; 20];
    a.copy_from_slice(&h[12..]);
    Address(a)
}

/// Ethereum address controlled by a raw secret key.
pub fn address_from_secret(sk: &[u8; 32]) -> Result<Address, String> {
    Identity::new(sk, 0).map(|i| i.address)
}

/// `keccak256("hlbft-handshake" || chain_id BE || nonce)`.
pub fn challenge_digest(chain_id: u64, nonce: &[u8; 32]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(DOMAIN);
    h.update(chain_id.to_be_bytes());
    h.update(nonce);
    h.finalize().into()
}

/// Sign a digest: 65 bytes r || s || v with v in {27, 28}.
pub fn sign_digest(sk: &SecretKey, digest: &[u8; 32]) -> [u8; 65] {
    let sig = SECP256K1.sign_ecdsa_recoverable(&Message::from_digest(*digest), sk);
    let (rid, rs) = sig.serialize_compact();
    let mut out = [0u8; 65];
    out[..64].copy_from_slice(&rs);
    out[64] = 27 + i32::from(rid) as u8;
    out
}

/// Recover the signer address of a 65-byte signature (v in 0/1 or 27/28).
pub fn recover_signer(digest: &[u8; 32], sig: &[u8; 65]) -> Option<Address> {
    let v = match sig[64] {
        0 | 1 => sig[64] as i32,
        27 | 28 => sig[64] as i32 - 27,
        _ => return None,
    };
    let rid = RecoveryId::try_from(v).ok()?;
    let rs = RecoverableSignature::from_compact(&sig[..64], rid).ok()?;
    let pk = SECP256K1.recover_ecdsa(&Message::from_digest(*digest), &rs).ok()?;
    Some(address_of(&pk))
}

fn encode_hello(id: &Identity, nonce: &[u8; 32]) -> [u8; HELLO_LEN] {
    let mut b = [0u8; HELLO_LEN];
    b[..8].copy_from_slice(MAGIC);
    b[8..12].copy_from_slice(&VERSION.to_be_bytes());
    b[12..20].copy_from_slice(&id.chain_id.to_be_bytes());
    b[20..40].copy_from_slice(&id.address.0);
    b[40..72].copy_from_slice(nonce);
    b
}

/// Run the handshake on `stream`. `accept(peer)` decides whether the claimed
/// peer address may connect (it is called before any signature is sent).
/// Returns the authenticated peer address.
pub async fn handshake<S, F>(stream: &mut S, id: &Identity, accept: F) -> Result<Address, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnOnce(&Address) -> bool,
{
    let our_nonce: [u8; 32] = rand::random();
    stream.write_all(&encode_hello(id, &our_nonce)).await?;
    stream.flush().await?;

    let mut hello = [0u8; HELLO_LEN];
    stream.read_exact(&mut hello).await?;
    if &hello[..8] != MAGIC || hello[8..12] != VERSION.to_be_bytes() {
        return Err(HandshakeError::BadMagic);
    }
    let theirs = u64::from_be_bytes(hello[12..20].try_into().unwrap());
    if theirs != id.chain_id {
        return Err(HandshakeError::ChainId { ours: id.chain_id, theirs });
    }
    let claimed = Address(hello[20..40].try_into().unwrap());
    let peer_nonce: [u8; 32] = hello[40..72].try_into().unwrap();
    if claimed == id.address || !accept(&claimed) {
        return Err(HandshakeError::NotAccepted(claimed));
    }

    let sig = sign_digest(&id.secret, &challenge_digest(id.chain_id, &peer_nonce));
    stream.write_all(&sig).await?;
    stream.flush().await?;

    let mut their_sig = [0u8; 65];
    stream.read_exact(&mut their_sig).await?;
    match recover_signer(&challenge_digest(id.chain_id, &our_nonce), &their_sig) {
        Some(a) if a == claimed => Ok(claimed),
        _ => Err(HandshakeError::BadSignature(claimed)),
    }
}

/// Handshake with a forged identity: claims `claimed` but signs with
/// `id.secret`. Only for tests of the rejection path.
#[doc(hidden)]
pub async fn handshake_claiming<S>(stream: &mut S, id: &Identity, claimed: Address) -> Result<Address, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let forged = Identity { secret: id.secret, address: claimed, chain_id: id.chain_id };
    handshake(stream, &forged, |_| true).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_recover_roundtrip() {
        let id = Identity::new(&[7u8; 32], 1).unwrap();
        let d = challenge_digest(1, &[3u8; 32]);
        let s = sign_digest(&id.secret, &d);
        assert_eq!(recover_signer(&d, &s), Some(id.address));
        let mut s01 = s;
        s01[64] -= 27;
        assert_eq!(recover_signer(&d, &s01), Some(id.address));
        let d2 = challenge_digest(2, &[3u8; 32]);
        assert_ne!(recover_signer(&d2, &s), Some(id.address));
    }

    #[test]
    fn known_address() {
        // Well-known: secret key 0x...01 -> 0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf
        let mut sk = [0u8; 32];
        sk[31] = 1;
        assert_eq!(
            address_from_secret(&sk).unwrap().to_hex(),
            "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf"
        );
    }

    #[tokio::test]
    async fn duplex_handshake_ok_and_bad() {
        let a = Identity::new(&[1u8; 32], 9).unwrap();
        let b = Identity::new(&[2u8; 32], 9).unwrap();
        let (mut x, mut y) = tokio::io::duplex(1024);
        let (ra, rb) = tokio::join!(handshake(&mut x, &a, |_| true), handshake(&mut y, &b, |_| true));
        assert_eq!(ra.unwrap(), b.address);
        assert_eq!(rb.unwrap(), a.address);

        // b claims to be c but signs with b's key.
        let c = Identity::new(&[3u8; 32], 9).unwrap();
        let (mut x, mut y) = tokio::io::duplex(1024);
        let (ra, _rb) = tokio::join!(handshake(&mut x, &a, |_| true), handshake_claiming(&mut y, &b, c.address));
        assert!(matches!(ra, Err(HandshakeError::BadSignature(p)) if p == c.address));

        // chain id mismatch
        let d = Identity::new(&[4u8; 32], 10).unwrap();
        let (mut x, mut y) = tokio::io::duplex(1024);
        let (ra, _) = tokio::join!(handshake(&mut x, &a, |_| true), handshake(&mut y, &d, |_| true));
        assert!(matches!(ra, Err(HandshakeError::ChainId { .. })));
    }
}
