//! Node-level contracts: application (state machine) <-> consensus <->
//! network/mempool <-> API.

use crate::block::*;
use crate::components::{AccountView, MarketCtx};
use crate::error::*;
use crate::exec::*;
use crate::fixed::*;
use crate::genesis::Genesis;
use crate::market::*;
use crate::primitives::*;
use crate::tx::*;
use borsh::{BorshDeserialize, BorshSerialize};

// ------------------------------------------------------------ application ---

/// Full state snapshot at an executed height.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Snapshot {
    pub height: Height,
    pub state_root: Hash32,
    /// zstd-compressed canonical state encoding.
    pub data: Vec<u8>,
}

/// The replicated state machine. Implemented by `state::Chain`.
/// Execution happens only for committed blocks, strictly in height order.
pub trait Application: Send {
    fn genesis(&self) -> &Genesis;
    /// Height of the last executed (committed) block; 0 = genesis.
    fn executed_height(&self) -> Height;
    /// State root after executing `h` (kept for a bounded window, at least
    /// the last 10_000 heights).
    fn root_at(&self, h: Height) -> Option<Hash32>;
    /// Execute a committed block. Must be deterministic: same (state, ctx,
    /// txs) => same outcome and root on every machine.
    fn execute_block(&mut self, ctx: &BlockContext, txs: &[Tx]) -> BlockOutcome;
    /// Cheap admission check for the mempool (signature, decode, nonce
    /// window, known signer). Must not mutate state. Returns the recovered
    /// signer (user txs: the signing key's address, before agent resolution;
    /// validator txs: the validator) so the mempool can rate-limit per signer.
    fn check_tx(&self, tx: &Tx) -> Result<Address, AdmissionError>;
    fn snapshot(&self) -> Snapshot;
    /// Replace state with a snapshot; returns the recomputed root, which the
    /// caller must compare against a certified `exec_root`.
    fn restore(&mut self, s: &Snapshot) -> Result<Hash32, String>;
    /// Read-only query surface for the API.
    fn query(&self) -> &dyn StateQuery;
}

// ---------------------------------------------------------------- queries ---

/// Current-state queries used by `crates/api` (/info). Historical data
/// (fills, order history, candles, funding history) is NOT here: the API
/// indexes it from `BlockOutcome.events`.
pub trait StateQuery: Send + Sync {
    fn markets(&self) -> Vec<MarketSpec>;
    fn market_ctx(&self, asset: AssetId) -> MarketCtx;
    fn mid(&self, asset: AssetId) -> Option<Px>;
    /// (bids, asks), each best-first: (px, sz, n_orders).
    fn l2_book(&self, asset: AssetId, depth: usize) -> (Vec<(Px, Sz, u32)>, Vec<(Px, Sz, u32)>);
    fn account(&self, user: &Address) -> AccountView;
    /// Resting + trigger orders of a user.
    fn open_orders(&self, user: &Address) -> Vec<RestingOrderView>;
    fn order_by_oid(&self, user: &Address, oid: Oid) -> Option<RestingOrderView>;
    fn order_by_cloid(&self, user: &Address, cloid: &Cloid) -> Option<RestingOrderView>;
    /// Agent wallet -> principal resolution.
    fn principal_of(&self, signer: &Address) -> Address;
    fn block_time(&self) -> TimestampMs;
    fn height(&self) -> Height;
    /// Pending bridge withdrawals with collected validator signatures.
    fn pending_withdrawals(&self) -> Vec<PendingWithdrawal>;
    fn processed_deposit(&self, deposit_id: u64) -> bool;
    fn validator_set(&self) -> ValidatorSet;
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, serde::Serialize, serde::Deserialize)]
pub struct PendingWithdrawal {
    pub id: u64,
    pub user: Address,
    pub destination: Address,
    pub amount: Usd,
    pub created_at: TimestampMs,
    /// Validator signatures over the EIP-712 withdrawal message.
    pub signatures: Vec<(Address, Sig65)>,
}

// ------------------------------------------------------------- tx decoding ---

/// Signature/encoding scheme for HL user transactions. Implemented by
/// `hlwire::HlVerifier`; the state machine is generic over it so it can be
/// tested without real signatures.
pub trait TxVerifier: Send + Sync {
    /// Decode `action_bytes` (msgpack) into a typed action.
    fn decode(&self, tx: &UserTx) -> Result<Action, String>;
    /// Recover the signing address (L1 phantom-agent scheme or user-signed
    /// EIP-712 scheme, depending on the action type).
    fn recover_signer(&self, tx: &UserTx, action: &Action) -> Result<Address, String>;
    /// Verify a validator tx signature; returns the signer.
    fn recover_validator(&self, tx: &ValidatorTx, chain_id: u64) -> Result<Address, String>;
}

// --------------------------------------------------------------- network ---

/// Peer-to-peer transport. Implemented by `net::P2p`.
/// Inbound messages are delivered on the channel returned at construction as
/// `(from, NetMsg)` where `from` is the authenticated validator address.
pub trait Network: Send + Sync {
    fn local(&self) -> Address;
    fn send(&self, to: Address, msg: NetMsg);
    fn broadcast(&self, msg: NetMsg);
    /// Currently connected peers.
    fn peers(&self) -> Vec<Address>;
}

/// Where a tx entered the node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxSource {
    /// From the local API; `ip` used for rate limiting.
    Api { ip: String },
    Peer(Address),
    /// Validator-internal (oracle, bridge relayer).
    Local,
}

/// Transaction pool. Implemented by `mempool::Pool`.
pub trait Mempool: Send + Sync {
    /// Admit a tx after `check` (stateless + Application::check_tx) and rate
    /// limits. Admitted txs from API/Local sources are gossiped to peers.
    fn insert(&self, tx: Tx, source: TxSource) -> Result<TxHash, AdmissionError>;
    /// Take up to the limits in FIFO order, validator txs first, without
    /// removing (they are removed when committed).
    fn reap(&self, max_txs: usize, max_bytes: usize) -> Vec<Tx>;
    /// Drop committed txs (and anything invalidated by them).
    fn remove_committed(&self, hashes: &[TxHash]);
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------- storage ---

/// Persistent consensus storage (blocks, QCs, safety state). Implemented in
/// `crates/consensus` (file-backed). Writes of safety state MUST be durable
/// (fsync) before the corresponding vote/timeout is sent.
pub trait BlockStore: Send {
    fn put_block(&mut self, b: &Block) -> std::io::Result<()>;
    fn get_block(&self, id: &BlockId) -> Option<Block>;
    fn put_committed(&mut self, c: &CommittedBlock) -> std::io::Result<()>;
    fn committed_at(&self, h: Height) -> Option<CommittedBlock>;
    fn committed_height(&self) -> Height;
    fn save_safety(&mut self, s: &SafetyState) -> std::io::Result<()>;
    fn load_safety(&self) -> Option<SafetyState>;
    fn save_snapshot(&mut self, s: &Snapshot) -> std::io::Result<()>;
    fn latest_snapshot(&self) -> Option<Snapshot>;
}

/// Persisted voting state — guarantees a restarted validator never votes
/// twice in a view or against its lock.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SafetyState {
    pub last_voted_view: View,
    pub last_timeout_view: View,
    pub locked_qc: QuorumCert,
    pub high_qc: QuorumCert,
}

// -------------------------------------------------------------- consensus ---

/// Inputs to the sans-IO consensus core.
#[derive(Clone, Debug)]
pub enum ConsensusInput {
    Message { from: Address, msg: ConsensusMsg },
    /// Called periodically and when a timer set via `SetTimer` expires.
    Tick,
    /// The application finished executing a committed height.
    Executed { height: Height, root: Hash32 },
}

#[derive(Clone, Debug)]
pub enum ConsensusOutput {
    Send { to: Address, msg: ConsensusMsg },
    Broadcast(ConsensusMsg),
    /// Hand a newly committed block to the executor (strictly increasing height).
    Commit(CommittedBlock),
    /// Request a wake-up at this wall-clock time.
    SetTimer { at_ms: u64 },
    /// Install a snapshot (state sync). Executor replies with `Executed`.
    InstallSnapshot(Snapshot),
}

/// Services the consensus core needs from the node.
pub trait ConsensusEnv {
    fn now_ms(&self) -> u64;
    /// Txs for a new proposal (from the mempool).
    fn payload(&mut self, max_txs: usize, max_bytes: usize) -> Vec<Tx>;
    /// (executed height, root) for the proposal header.
    fn exec_state(&self) -> (Height, Hash32);
    /// Root the local executor computed for `h`, if executed.
    fn root_at(&self, h: Height) -> Option<Hash32>;
    /// Latest local snapshot, to serve SnapshotRequest.
    fn snapshot(&self) -> Option<Snapshot>;
    /// Replay mode: if Some, the leader must propose exactly this
    /// (timestamp, payload) for `height` instead of mempool contents, and
    /// voters reject proposals that differ. Because commit needs two
    /// successors, the override must return two (empty) padding blocks past
    /// the end of the log for the last logged heights to commit.
    fn proposal_override(&mut self, _height: Height) -> Option<(TimestampMs, Vec<Tx>)> {
        None
    }
    fn sign(&self, digest: &[u8; 32]) -> Sig65;
    fn verify(&self, digest: &[u8; 32], sig: &Sig65, signer: &Address) -> bool;
    fn store(&mut self) -> &mut dyn BlockStore;
}

/// Sans-IO consensus state machine (deterministic given inputs + env), so it
/// can be driven by a simulated network in tests and by TCP in the node.
pub trait ConsensusCore {
    fn handle(&mut self, input: ConsensusInput, env: &mut dyn ConsensusEnv) -> Vec<ConsensusOutput>;
    fn committed_height(&self) -> Height;
    fn current_view(&self) -> View;
}

// -------------------------------------------------------------- node <-> api ---

/// Node status for `/admin/status` and harnesses.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeStatus {
    pub validator: Option<Address>,
    pub committed_height: Height,
    pub executed_height: Height,
    pub view: View,
    pub exec_root: Hash32,
    pub mempool_len: usize,
    pub peers: Vec<Address>,
    pub last_commit_ms: u64,
}

/// Callback receiving every executed block, in height order.
pub type BlockCallback = Box<dyn Fn(std::sync::Arc<BlockOutcome>) + Send + Sync>;

/// What the API server needs from the node. Implemented in `crates/node`
/// (and by an in-memory fake in api tests).
pub trait ApiBackend: Send + Sync + 'static {
    fn genesis(&self) -> Genesis;
    /// Run `f` against a consistent view of the latest executed state.
    fn query(&self, f: &mut dyn FnMut(&dyn StateQuery));
    /// Submit a tx to the local mempool (gossiped to peers).
    fn submit(&self, tx: Tx, source: TxSource) -> Result<TxHash, AdmissionError>;
    /// Register for executed-block notifications (receipts + events).
    fn on_block(&self, cb: BlockCallback);
    fn status(&self) -> NodeStatus;
    /// Apply a chaos config (JSON, see net::ChaosConfig). Admin only.
    fn set_chaos(&self, _cfg_json: &str) -> Result<(), String> {
        Err("chaos not supported".into())
    }
}

// ----------------------------------------------------------- node <-> bridge ---

/// What the in-node bridge watcher (crates/relayer) needs from its validator.
pub trait BridgeHost: Send + Sync + 'static {
    /// This validator's address (None for non-validators).
    fn validator(&self) -> Option<Address>;
    /// Sign & submit a validator action to the local mempool.
    fn submit_validator_action(&self, a: ValidatorAction) -> Result<(), String>;
    fn pending_withdrawals(&self) -> Vec<PendingWithdrawal>;
    fn processed_deposit(&self, deposit_id: u64) -> bool;
    /// Sign a 32-byte digest with the validator key (withdrawal EIP-712 digest).
    fn sign_digest(&self, digest: &[u8; 32]) -> Option<Sig65>;
    fn genesis(&self) -> Genesis;
}
