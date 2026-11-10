//! In-node bridge watcher (one per validator).
//!
//! * scans `Deposit` / `Withdrawn` logs of the bridge contract up to
//!   `head - confirmations`;
//! * submits `BridgeDeposit` once per deposit not yet `processed_deposit`;
//! * signs every pending withdrawal it has not signed yet and submits
//!   `WithdrawalSignature` once per (withdrawal, this validator);
//! * submits `WithdrawalFinalized` once a `Withdrawn` event for a pending
//!   withdrawal is final.
//!
//! All state is in memory and re-derivable: after a node restart the watcher
//! rescans from `start_block` and the host's `processed_deposit` /
//! `pending_withdrawals` views suppress duplicates. An Anvil restart (or any
//! reorg below the scan cursor) is detected by re-checking the hash of the
//! last scanned block; the watcher then rescans from `start_block`.
//! A submitted action that has not taken effect after `resubmit_after` is
//! submitted again (mempool drops, leader crashes), so the L1 must treat
//! duplicate validator actions idempotently.

use crate::contract::{decode_deposit, decode_withdrawn, deposit_topic, withdrawn_topic};
use crate::eip712::withdrawal_digest;
use crate::rpc::EthRpc;
use anyhow::Result;
use interfaces::{Address, BridgeHost, BridgeParams, DepositAttestation, Hash32, Usd, ValidatorAction};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct WatcherConfig {
    /// EVM JSON-RPC endpoint (Anvil: http://127.0.0.1:8545).
    pub rpc_url: String,
    /// A log is final once `log.block + confirmations <= head`.
    /// Note: Anvil without `--block-time` only mines on transactions, so
    /// confirmations > 0 need `anvil --block-time 1` (or other traffic).
    pub confirmations: u64,
    pub poll_interval: Duration,
    /// First block to scan (the bridge deployment block).
    pub start_block: u64,
    /// Max blocks per eth_getLogs request.
    pub max_block_range: u64,
    /// Re-submit an action that has not taken effect after this long.
    pub resubmit_after: Duration,
    /// Overrides `genesis.bridge` (contract address / chain id) if set.
    pub params_override: Option<BridgeParams>,
}

impl WatcherConfig {
    pub fn new(rpc_url: impl Into<String>) -> Self {
        Self {
            rpc_url: rpc_url.into(),
            confirmations: 1,
            poll_interval: Duration::from_millis(500),
            start_block: 0,
            max_block_range: 5_000,
            resubmit_after: Duration::from_secs(30),
            params_override: None,
        }
    }
}

/// What one `poll_once` did (for tests / metrics).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PollStats {
    pub scanned_to: Option<u64>,
    pub deposits_attested: Vec<u64>,
    pub withdrawals_signed: Vec<u64>,
    pub withdrawals_finalized: Vec<u64>,
    pub chain_reset: bool,
}

pub struct DepositWatcher {
    host: Arc<dyn BridgeHost>,
    rpc: EthRpc,
    cfg: WatcherConfig,
    params: BridgeParams,
    /// Next block to scan.
    cursor: u64,
    /// Last scanned (block, hash), for restart/reorg detection.
    anchor: Option<(u64, Hash32)>,
    /// Final deposits not yet processed by the L1.
    deposits: BTreeMap<u64, DepositAttestation>,
    deposit_submitted: BTreeMap<u64, Instant>,
    /// Withdrawal ids with a final `Withdrawn` event.
    finalized_seen: BTreeSet<u64>,
    finalized_submitted: BTreeMap<u64, Instant>,
    sig_submitted: BTreeMap<u64, Instant>,
}

fn due(map: &BTreeMap<u64, Instant>, id: u64, after: Duration) -> bool {
    map.get(&id).is_none_or(|t| t.elapsed() >= after)
}

impl DepositWatcher {
    pub fn new(host: Arc<dyn BridgeHost>, cfg: WatcherConfig) -> Self {
        let params = cfg.params_override.clone().unwrap_or_else(|| host.genesis().bridge);
        Self {
            rpc: EthRpc::new(cfg.rpc_url.clone()),
            cursor: cfg.start_block,
            host,
            cfg,
            params,
            anchor: None,
            deposits: BTreeMap::new(),
            deposit_submitted: BTreeMap::new(),
            finalized_seen: BTreeSet::new(),
            finalized_submitted: BTreeMap::new(),
            sig_submitted: BTreeMap::new(),
        }
    }

    pub fn params(&self) -> &BridgeParams {
        &self.params
    }

    /// Run forever (errors are logged and retried next tick).
    pub async fn run(mut self) {
        tracing::info!(contract = %self.params.contract, rpc = %self.cfg.rpc_url, "bridge watcher started");
        loop {
            if let Err(e) = self.poll_once().await {
                tracing::warn!("bridge watcher poll failed: {e:#}");
            }
            tokio::time::sleep(self.cfg.poll_interval).await;
        }
    }

    /// Spawn `run` on the current tokio runtime.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run())
    }

    fn reset(&mut self) {
        self.cursor = self.cfg.start_block;
        self.anchor = None;
        self.deposits.clear();
        self.deposit_submitted.clear();
        self.finalized_seen.clear();
        self.finalized_submitted.clear();
    }

    /// Scan new final blocks; returns whether a chain reset was detected.
    async fn scan(&mut self, stats: &mut PollStats) -> Result<()> {
        let head = self.rpc.block_number().await?;
        if let Some((n, h)) = self.anchor {
            let still = if head < n { None } else { self.rpc.block_hash(n).await? };
            if still != Some(h) {
                tracing::warn!(block = n, head, "bridge chain reset/reorg detected; rescanning");
                self.reset();
                stats.chain_reset = true;
            }
        }
        let Some(safe) = head.checked_sub(self.cfg.confirmations) else { return Ok(()) };
        let topics = [deposit_topic(), withdrawn_topic()];
        while self.cursor <= safe {
            let to = safe.min(self.cursor + self.cfg.max_block_range.max(1) - 1);
            let logs = self.rpc.get_logs(&self.params.contract, &topics, self.cursor, to).await?;
            let to_hash = self.rpc.block_hash(to).await?;
            for log in &logs {
                if log.topics.first() == Some(&topics[0]) {
                    match decode_deposit(log) {
                        Ok(d) => self.on_deposit(d),
                        Err(e) => tracing::warn!("undecodable Deposit log: {e:#}"),
                    }
                } else {
                    match decode_withdrawn(log) {
                        Ok(w) => {
                            self.finalized_seen.insert(w.id);
                        }
                        Err(e) => tracing::warn!("undecodable Withdrawn log: {e:#}"),
                    }
                }
            }
            match to_hash {
                Some(h) => self.anchor = Some((to, h)),
                None => anyhow::bail!("block {to} vanished while scanning"),
            }
            self.cursor = to + 1;
            stats.scanned_to = Some(to);
        }
        Ok(())
    }

    fn on_deposit(&mut self, d: crate::contract::DepositEvent) {
        let Ok(raw) = i64::try_from(d.amount) else {
            tracing::warn!(id = d.id, amount = d.amount, "deposit amount does not fit Usd; ignored");
            return;
        };
        if self.host.processed_deposit(d.id) {
            return;
        }
        self.deposits.entry(d.id).or_insert(DepositAttestation {
            deposit_id: d.id,
            user: d.user,
            amount: Usd::from_raw(raw as i128),
            eth_tx_hash: d.tx_hash,
        });
    }

    fn submit(&self, a: ValidatorAction) -> bool {
        match self.host.submit_validator_action(a.clone()) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(?a, "submit_validator_action failed: {e}");
                false
            }
        }
    }

    /// One iteration: scan, then submit whatever is due.
    pub async fn poll_once(&mut self) -> Result<PollStats> {
        let mut stats = PollStats::default();
        let Some(me) = self.host.validator() else { return Ok(stats) };
        let scan_result = self.scan(&mut stats).await;
        // Even if the RPC failed we can still act on what we already know.
        self.act(me, &mut stats);
        scan_result.map(|_| stats)
    }

    fn act(&mut self, me: Address, stats: &mut PollStats) {
        let after = self.cfg.resubmit_after;
        // ---- deposits
        let ids: Vec<u64> = self.deposits.keys().copied().collect();
        for id in ids {
            if self.host.processed_deposit(id) {
                self.deposits.remove(&id);
                self.deposit_submitted.remove(&id);
                continue;
            }
            if due(&self.deposit_submitted, id, after) {
                let att = self.deposits[&id].clone();
                if self.submit(ValidatorAction::BridgeDeposit(att)) {
                    tracing::info!(id, "attested bridge deposit");
                    self.deposit_submitted.insert(id, Instant::now());
                    stats.deposits_attested.push(id);
                }
            }
        }
        // ---- withdrawals
        let pending = self.host.pending_withdrawals();
        let pending_ids: BTreeSet<u64> = pending.iter().map(|w| w.id).collect();
        self.sig_submitted.retain(|id, _| pending_ids.contains(id));
        self.finalized_submitted.retain(|id, _| pending_ids.contains(id));
        for w in &pending {
            if self.finalized_seen.contains(&w.id) {
                if due(&self.finalized_submitted, w.id, after)
                    && self.submit(ValidatorAction::WithdrawalFinalized { withdrawal_id: w.id })
                {
                    tracing::info!(id = w.id, "attested withdrawal finalized");
                    self.finalized_submitted.insert(w.id, Instant::now());
                    stats.withdrawals_finalized.push(w.id);
                }
                continue;
            }
            if w.signatures.iter().any(|(a, _)| *a == me) || !due(&self.sig_submitted, w.id, after) {
                continue;
            }
            if w.amount.raw() <= 0 {
                tracing::warn!(id = w.id, "pending withdrawal with non-positive amount; not signing");
                continue;
            }
            let digest = withdrawal_digest(w, &self.params);
            let Some(signature) = self.host.sign_digest(&digest) else {
                tracing::warn!(id = w.id, "host refused to sign withdrawal digest");
                continue;
            };
            if self.submit(ValidatorAction::WithdrawalSignature { withdrawal_id: w.id, signature }) {
                tracing::info!(id = w.id, "signed withdrawal");
                self.sig_submitted.insert(w.id, Instant::now());
                stats.withdrawals_signed.push(w.id);
            }
        }
    }
}
