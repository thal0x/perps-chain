use interfaces::*;

/// Static configuration of a [`crate::Core`].
#[derive(Clone, Debug)]
pub struct CoreConfig {
    pub chain_id: u64,
    /// Genesis block timestamp (see [`crate::genesis_block`]).
    pub genesis_time_ms: TimestampMs,
    pub validators: ValidatorSet,
    /// This node's address. A node not in `validators` runs as an observer
    /// (follows commits, never votes/proposes).
    pub me: Address,
    /// Minimum time between parent timestamp and a new proposal.
    pub min_block_interval_ms: u64,
    /// View timeout: base, multiplied by 1.5 per consecutive TC-ended view,
    /// capped at `max_timeout_ms`; reset when a view ends with a QC.
    pub base_timeout_ms: u64,
    pub max_timeout_ms: u64,
    /// Interval for re-broadcasting our timeout while stuck in a view.
    pub timeout_rebroadcast_ms: u64,
    /// Proposals must not be further than this ahead of the voter's clock.
    pub max_future_drift_ms: u64,
    pub max_block_txs: usize,
    pub max_block_bytes: usize,
    /// `Status` beacon interval.
    pub status_interval_ms: u64,
    /// Max blocks per `SyncResponse` (soft; see README).
    pub sync_batch: u64,
    /// Max total txs per `SyncResponse` (keeps frames under the net limit).
    pub sync_max_txs: usize,
    /// Request block sync when a peer is this many heights ahead (or when
    /// our commits stalled for `sync_stall_ms` and the peer is ahead at all).
    pub sync_gap: u64,
    pub sync_stall_ms: u64,
    /// Fetch missing ancestors individually only within this many heights
    /// of our committed height; beyond, use block sync.
    pub fetch_window: u64,
    /// In-memory blocks kept below the committed height.
    pub memory_window: u64,
}

impl CoreConfig {
    pub fn new(chain_id: u64, genesis_time_ms: TimestampMs, validators: ValidatorSet, me: Address) -> Self {
        CoreConfig {
            chain_id,
            genesis_time_ms,
            validators,
            me,
            min_block_interval_ms: 200,
            base_timeout_ms: 1000,
            max_timeout_ms: 10_000,
            timeout_rebroadcast_ms: 1000,
            max_future_drift_ms: 2000,
            max_block_txs: 20_000,
            max_block_bytes: 16 << 20,
            status_interval_ms: 1000,
            sync_batch: 256,
            sync_max_txs: 60_000,
            sync_gap: 3,
            sync_stall_ms: 2000,
            fetch_window: 64,
            memory_window: 64,
        }
    }

    /// Timeout for a view entered after `streak` consecutive TC-ended views.
    pub fn view_timeout(&self, streak: u32) -> u64 {
        let mut d = self.base_timeout_ms;
        for _ in 0..streak {
            d = d.saturating_mul(3) / 2;
            if d >= self.max_timeout_ms {
                return self.max_timeout_ms;
            }
        }
        d.min(self.max_timeout_ms)
    }
}
