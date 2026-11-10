//! Blocks and consensus messages (pipelined HotStuff family; see
//! ARCHITECTURE.md §Consensus for the exact protocol).
//!
//! All consensus signatures are secp256k1 over
//! `keccak256(domain_tag || borsh(signed_payload))` with the validator's key;
//! the signer is identified by its Ethereum address.

use crate::primitives::*;
use crate::tx::Tx;
use borsh::{BorshDeserialize, BorshSerialize};

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct BlockHeader {
    pub chain_id: u64,
    pub height: Height,
    pub view: View,
    pub parent: BlockId,
    pub proposer: Address,
    /// Proposer wall clock; must be > parent.timestamp_ms and
    /// <= voter_now + MAX_FUTURE_DRIFT_MS (2000) for a voter to vote.
    pub timestamp_ms: TimestampMs,
    /// blake3(borsh(payload)).
    pub payload_hash: Hash32,
    /// Latest height the proposer had executed when proposing, and its state
    /// root. Voters that have executed `exec_height` refuse to vote if their
    /// root differs (state divergence alarm). Certifies roots for state sync.
    pub exec_height: Height,
    pub exec_root: Hash32,
    /// QC for `parent` (or for the highest certified block if the proposal
    /// follows a timeout).
    pub justify: QuorumCert,
}

impl BlockHeader {
    pub fn id(&self) -> BlockId {
        hash_borsh(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Block {
    pub header: BlockHeader,
    pub payload: Vec<Tx>,
}

impl Block {
    pub fn id(&self) -> BlockId {
        self.header.id()
    }
    pub fn payload_hash(payload: &[Tx]) -> Hash32 {
        hash_borsh(&payload.to_vec())
    }
}

/// Vote signed over `VoteData`.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct VoteData {
    pub chain_id: u64,
    pub block_id: BlockId,
    pub view: View,
    pub height: Height,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Vote {
    pub data: VoteData,
    pub voter: Address,
    pub signature: Sig65,
}

/// Quorum certificate: >= 2f+1 stake-weighted votes on the same VoteData.
/// The genesis QC has `view == 0`, `height == 0`, `block_id == genesis id`
/// and no signatures.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct QuorumCert {
    pub data: VoteData,
    pub signatures: Vec<(Address, Sig65)>,
}

/// Signed when a validator's view timer expires.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct TimeoutData {
    pub chain_id: u64,
    pub view: View,
    /// View of the sender's highest QC.
    pub high_qc_view: View,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct TimeoutMsg {
    pub data: TimeoutData,
    pub high_qc: QuorumCert,
    pub sender: Address,
    pub signature: Sig65,
}

/// >= 2f+1 timeouts for the same view. Next leader extends the highest QC
/// among them.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct TimeoutCert {
    pub view: View,
    pub timeouts: Vec<(Address, View, Sig65)>,
    pub high_qc: QuorumCert,
}

/// Committed block with the certificate chain proving it committed.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct CommittedBlock {
    pub block: Block,
    /// QC(s) whose existence implies the commit rule fired for `block`
    /// (e.g. for 2-chain: QC on block and QC on its direct child).
    pub commit_proof: Vec<QuorumCert>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum ConsensusMsg {
    Proposal { block: Block, tc: Option<TimeoutCert> },
    Vote(Vote),
    Timeout(TimeoutMsg),
    /// Ask a peer for committed blocks in [from, to] (inclusive, bounded).
    SyncRequest { from: Height, to: Height },
    SyncResponse { blocks: Vec<CommittedBlock>, peer_committed_height: Height },
    /// Ask a peer for a block by id (missing parent during normal operation).
    FetchBlock { id: BlockId },
    FetchBlockResponse { block: Option<Block> },
    /// State snapshot sync for nodes too far behind to replay blocks.
    SnapshotRequest,
    SnapshotResponse(Option<crate::traits::Snapshot>),
    /// Periodic liveness/status beacon (lets laggards learn they are behind).
    Status { committed_height: Height, high_qc_view: View },
}

/// Everything sent between nodes.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum NetMsg {
    Consensus(ConsensusMsg),
    /// Mempool gossip.
    Txs(Vec<Tx>),
}

/// Validator entry in the active set.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, serde::Serialize, serde::Deserialize)]
pub struct ValidatorInfo {
    pub address: Address,
    /// Voting power. Quorum = smallest stake sum > 2/3 of total.
    pub stake: u64,
    /// host:port for p2p.
    pub p2p_addr: String,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, serde::Serialize, serde::Deserialize)]
pub struct ValidatorSet {
    pub validators: Vec<ValidatorInfo>,
}

impl ValidatorSet {
    pub fn total_stake(&self) -> u64 {
        self.validators.iter().map(|v| v.stake).sum()
    }
    /// Minimum stake for a quorum: floor(2*total/3) + 1.
    pub fn quorum_stake(&self) -> u64 {
        self.total_stake() * 2 / 3 + 1
    }
    /// Minimum stake guaranteed to include one honest validator: floor(total/3) + 1.
    pub fn validity_stake(&self) -> u64 {
        self.total_stake() / 3 + 1
    }
    pub fn stake_of(&self, a: &Address) -> u64 {
        self.validators.iter().find(|v| &v.address == a).map(|v| v.stake).unwrap_or(0)
    }
    pub fn contains(&self, a: &Address) -> bool {
        self.validators.iter().any(|v| &v.address == a)
    }
    /// Round-robin leader for a view (stake-agnostic, deterministic).
    pub fn leader(&self, view: View) -> Address {
        self.validators[(view % self.validators.len() as u64) as usize].address
    }
}
