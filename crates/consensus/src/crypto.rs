//! Signing digests and certificate verification.
//!
//! Every consensus signature is over
//! `keccak256(domain_tag || borsh(signed_payload))`.
//!
//! Votes use one of two domain tags depending on how the voted block extends
//! its justify QC:
//! * [`VOTE_DIRECT_TAG`] when `block.justify.view + 1 == block.view`
//!   (the "happy path"),
//! * [`VOTE_TC_TAG`] when the block extends its QC through a timeout
//!   certificate.
//!
//! All honest voters of a given block use the same tag (it is a property of
//! the block), so a QC is uniformly of one [`VoteKind`]. This makes commit
//! proofs verifiable from QCs alone: a `Direct` QC for B' at view `v+1`
//! together with a QC for B at view `v` proves `B'.parent == B` (B' justify
//! was a QC at view v, and at most one block per view can be certified), so
//! the 2-chain commit rule fired for B. See `verify_direct_commit_proof`.

use interfaces::*;
use sha3::{Digest, Keccak256};

pub const VOTE_DIRECT_TAG: &[u8] = b"hlbft/consensus/vote/direct/v1";
pub const VOTE_TC_TAG: &[u8] = b"hlbft/consensus/vote/tc/v1";
pub const TIMEOUT_TAG: &[u8] = b"hlbft/consensus/timeout/v1";

/// How the certified block extended its parent QC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum VoteKind {
    /// `justify.view + 1 == view`.
    Direct,
    /// Extended through a timeout certificate.
    AfterTimeout,
}

impl VoteKind {
    pub fn of_header(h: &BlockHeader) -> VoteKind {
        if h.justify.data.view + 1 == h.view {
            VoteKind::Direct
        } else {
            VoteKind::AfterTimeout
        }
    }
    fn tag(self) -> &'static [u8] {
        match self {
            VoteKind::Direct => VOTE_DIRECT_TAG,
            VoteKind::AfterTimeout => VOTE_TC_TAG,
        }
    }
}

fn keccak_tagged<T: borsh::BorshSerialize>(tag: &[u8], v: &T) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(tag);
    h.update(borsh::to_vec(v).expect("borsh to vec"));
    h.finalize().into()
}

pub fn vote_digest(d: &VoteData, kind: VoteKind) -> [u8; 32] {
    keccak_tagged(kind.tag(), d)
}

pub fn timeout_digest(d: &TimeoutData) -> [u8; 32] {
    keccak_tagged(TIMEOUT_TAG, d)
}

pub fn keccak256(bytes: &[u8]) -> [u8; 32] {
    Keccak256::digest(bytes).into()
}

/// Signature verification callback (`ConsensusEnv::verify` shape).
pub type VerifyFn<'a> = &'a dyn Fn(&[u8; 32], &Sig65, &Address) -> bool;

// ------------------------------------------------------------------ genesis --

/// The pseudo-QC stored in the genesis header (points at the zero id).
pub fn pre_genesis_qc(chain_id: u64) -> QuorumCert {
    QuorumCert { data: VoteData { chain_id, block_id: Hash32::ZERO, view: 0, height: 0 }, signatures: vec![] }
}

/// Deterministic genesis block (height 0, view 0, empty payload). Every node
/// of a chain must use the same `(chain_id, genesis_time_ms)`.
pub fn genesis_block(chain_id: u64, genesis_time_ms: TimestampMs) -> Block {
    let payload: Vec<Tx> = vec![];
    Block {
        header: BlockHeader {
            chain_id,
            height: 0,
            view: 0,
            parent: Hash32::ZERO,
            proposer: Address::ZERO,
            timestamp_ms: genesis_time_ms,
            payload_hash: hash_borsh(&payload),
            exec_height: 0,
            exec_root: Hash32::ZERO,
            justify: pre_genesis_qc(chain_id),
        },
        payload,
    }
}

/// The (unsigned) genesis QC: view 0, height 0, genesis id.
pub fn genesis_qc(chain_id: u64, genesis_id: BlockId) -> QuorumCert {
    QuorumCert { data: VoteData { chain_id, block_id: genesis_id, view: 0, height: 0 }, signatures: vec![] }
}

// ------------------------------------------------------------ verification --

/// Context needed to verify certificates.
pub struct CertCtx<'a> {
    pub chain_id: u64,
    pub genesis_id: BlockId,
    pub validators: &'a ValidatorSet,
}

impl CertCtx<'_> {
    pub fn is_genesis_qc(&self, qc: &QuorumCert) -> bool {
        qc.signatures.is_empty()
            && qc.data.view == 0
            && qc.data.height == 0
            && qc.data.chain_id == self.chain_id
            && qc.data.block_id == self.genesis_id
    }

    /// Verify a QC: distinct validator signers holding quorum stake, all
    /// signatures valid under one vote kind. Returns that kind.
    pub fn verify_qc(&self, qc: &QuorumCert, verify: VerifyFn) -> Result<VoteKind, String> {
        if self.is_genesis_qc(qc) {
            return Ok(VoteKind::Direct);
        }
        if qc.data.chain_id != self.chain_id {
            return Err("qc: wrong chain id".into());
        }
        if qc.data.view == 0 {
            return Err("qc: view 0 must be genesis".into());
        }
        let mut stake = 0u64;
        let mut seen: Vec<Address> = Vec::with_capacity(qc.signatures.len());
        for (a, _) in &qc.signatures {
            if seen.contains(a) {
                return Err("qc: duplicate signer".into());
            }
            let s = self.validators.stake_of(a);
            if s == 0 {
                return Err("qc: non-validator signer".into());
            }
            seen.push(*a);
            stake = stake.checked_add(s).expect("stake overflow");
        }
        if stake < self.validators.quorum_stake() {
            return Err("qc: insufficient stake".into());
        }
        let (a0, s0) = &qc.signatures[0];
        let kind = [VoteKind::Direct, VoteKind::AfterTimeout]
            .into_iter()
            .find(|k| verify(&vote_digest(&qc.data, *k), s0, a0))
            .ok_or_else(|| "qc: bad signature".to_string())?;
        let digest = vote_digest(&qc.data, kind);
        for (a, s) in &qc.signatures[1..] {
            if !verify(&digest, s, a) {
                return Err("qc: bad signature".into());
            }
        }
        Ok(kind)
    }

    /// Verify a TC (signatures, quorum, `high_qc` consistent with the
    /// maximum signed high-QC view). Does not verify `tc.high_qc` itself.
    pub fn verify_tc_sigs(&self, tc: &TimeoutCert, verify: VerifyFn) -> Result<(), String> {
        let mut stake = 0u64;
        let mut seen: Vec<Address> = Vec::new();
        let mut max_hqc = 0u64;
        for (a, hqc_view, sig) in &tc.timeouts {
            if seen.contains(a) {
                return Err("tc: duplicate signer".into());
            }
            let s = self.validators.stake_of(a);
            if s == 0 {
                return Err("tc: non-validator signer".into());
            }
            if *hqc_view >= tc.view {
                return Err("tc: high qc view >= tc view".into());
            }
            let d = timeout_digest(&TimeoutData { chain_id: self.chain_id, view: tc.view, high_qc_view: *hqc_view });
            if !verify(&d, sig, a) {
                return Err("tc: bad signature".into());
            }
            seen.push(*a);
            stake = stake.checked_add(s).expect("stake overflow");
            max_hqc = max_hqc.max(*hqc_view);
        }
        if stake < self.validators.quorum_stake() {
            return Err("tc: insufficient stake".into());
        }
        if tc.high_qc.data.view != max_hqc {
            return Err("tc: high_qc is not the max signed high qc".into());
        }
        Ok(())
    }

    pub fn verify_tc(&self, tc: &TimeoutCert, verify: VerifyFn) -> Result<(), String> {
        self.verify_tc_sigs(tc, verify)?;
        self.verify_qc(&tc.high_qc, verify).map(|_| ())
    }

    /// A "direct" commit proof `[QC(B), QC(B')]` where B' is a direct child of
    /// B at view `B.view + 1`. Proves B committed on its own.
    pub fn verify_direct_commit_proof(&self, block_id: &BlockId, height: Height, proof: &[QuorumCert], verify: VerifyFn) -> Result<(), String> {
        if proof.len() != 2 {
            return Err("proof: not a direct proof".into());
        }
        let (q0, q1) = (&proof[0], &proof[1]);
        if q0.data.block_id != *block_id || q0.data.height != height {
            return Err("proof: qc0 does not certify block".into());
        }
        if q1.data.view != q0.data.view + 1 || q1.data.height != q0.data.height + 1 {
            return Err("proof: qc1 not a consecutive child".into());
        }
        self.verify_qc(q0, verify)?;
        match self.verify_qc(q1, verify)? {
            VoteKind::Direct => Ok(()),
            VoteKind::AfterTimeout => Err("proof: child extended via TC".into()),
        }
    }

    /// Verify a run of consecutive committed blocks.
    ///
    /// * Every block's payload hash and header linkage (`parent` == previous
    ///   id, consecutive heights) is checked, and the QC on the block itself
    ///   (`commit_proof[0]`) is verified.
    /// * A block is proven committed if it carries a valid direct proof, or if
    ///   the next block in the run is proven committed and links to it.
    /// * With `anchor = Some((h, id))` the first block must be at `h+1` with
    ///   parent `id`.
    ///
    /// Returns the number of leading blocks proven committed (blocks above the
    /// highest direct proof cannot be proven and are not counted).
    pub fn verify_committed_run(&self, blocks: &[CommittedBlock], anchor: Option<(Height, BlockId)>, verify: VerifyFn) -> usize {
        let mut ids = Vec::with_capacity(blocks.len());
        let mut ok_prefix = 0usize;
        for (i, cb) in blocks.iter().enumerate() {
            let h = &cb.block.header;
            if h.chain_id != self.chain_id {
                break;
            }
            let id = h.id();
            if i == 0 {
                if let Some((ah, aid)) = anchor {
                    if h.height != ah + 1 || h.parent != aid {
                        break;
                    }
                }
            } else {
                let prev = &blocks[i - 1].block.header;
                if h.height != prev.height + 1 || h.parent != ids[i - 1] || h.timestamp_ms <= prev.timestamp_ms {
                    break;
                }
            }
            if h.justify.data.block_id != h.parent || h.justify.data.height + 1 != h.height {
                break;
            }
            if cb.commit_proof.is_empty() || cb.commit_proof[0].data.block_id != id || cb.commit_proof[0].data.height != h.height {
                break;
            }
            if hash_borsh(&cb.block.payload) != h.payload_hash {
                break;
            }
            if self.verify_qc(&cb.commit_proof[0], verify).is_err() {
                break;
            }
            ids.push(id);
            ok_prefix = i + 1;
        }
        // find highest block in the valid prefix with a direct proof
        for i in (0..ok_prefix).rev() {
            let cb = &blocks[i];
            if cb.commit_proof.len() == 2
                && self.verify_direct_commit_proof(&ids[i], cb.block.header.height, &cb.commit_proof, verify).is_ok()
            {
                return i + 1;
            }
        }
        0
    }
}

// --------------------------------------------------------------- secp256k1 --

/// Real secp256k1 helpers (Ethereum-style recoverable signatures) for nodes
/// and tests implementing `ConsensusEnv::sign/verify`.
pub mod secp {
    use super::keccak256;
    use interfaces::{Address, Sig65};
    use secp256k1::ecdsa::{RecoverableSignature, RecoveryId};
    use secp256k1::{Message, PublicKey, SecretKey, SECP256K1};

    pub fn address_of(pk: &PublicKey) -> Address {
        let ser = pk.serialize_uncompressed();
        let h = keccak256(&ser[1..]);
        let mut a = [0u8; 20];
        a.copy_from_slice(&h[12..]);
        Address(a)
    }

    pub fn address_of_secret(sk: &SecretKey) -> Address {
        address_of(&PublicKey::from_secret_key(SECP256K1, sk))
    }

    pub fn sign(sk: &SecretKey, digest: &[u8; 32]) -> Sig65 {
        let msg = Message::from_digest(*digest);
        let sig = SECP256K1.sign_ecdsa_recoverable(&msg, sk);
        let (rid, bytes) = sig.serialize_compact();
        let mut r = [0u8; 32];
        let mut s = [0u8; 32];
        r.copy_from_slice(&bytes[..32]);
        s.copy_from_slice(&bytes[32..]);
        Sig65 { r, s, v: 27 + i32::from(rid) as u8 }
    }

    pub fn recover(digest: &[u8; 32], sig: &Sig65) -> Option<Address> {
        let v = match sig.v {
            27 | 28 => sig.v - 27,
            0 | 1 => sig.v,
            _ => return None,
        };
        let rid = RecoveryId::try_from(v as i32).ok()?;
        let mut bytes = [0u8; 64];
        bytes[..32].copy_from_slice(&sig.r);
        bytes[32..].copy_from_slice(&sig.s);
        let rs = RecoverableSignature::from_compact(&bytes, rid).ok()?;
        let pk = SECP256K1.recover_ecdsa(&Message::from_digest(*digest), &rs).ok()?;
        Some(address_of(&pk))
    }

    pub fn verify(digest: &[u8; 32], sig: &Sig65, signer: &Address) -> bool {
        recover(digest, sig).as_ref() == Some(signer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secp_roundtrip() {
        let sk = secp256k1::SecretKey::from_slice(&[7u8; 32]).unwrap();
        let a = secp::address_of_secret(&sk);
        let d = keccak256(b"hello");
        let s = secp::sign(&sk, &d);
        assert!(secp::verify(&d, &s, &a));
        let d2 = keccak256(b"hellO");
        assert!(!secp::verify(&d2, &s, &a));
        let mut s0 = s;
        s0.v -= 27;
        assert!(secp::verify(&d, &s0, &a));
    }

    #[test]
    fn domain_tags_differ() {
        let d = VoteData { chain_id: 1, block_id: Hash32([1; 32]), view: 3, height: 2 };
        assert_ne!(vote_digest(&d, VoteKind::Direct), vote_digest(&d, VoteKind::AfterTimeout));
    }
}
