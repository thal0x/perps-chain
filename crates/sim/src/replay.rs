//! `sim replay`: offline re-execution of a block log through the embedded
//! application with invariant checks and per-block roots.

use crate::app::{AppFactory, AppOptions};
use crate::log::LogReader;
use interfaces::*;
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvariantEvery {
    Never,
    Every,
    N(u64),
}

impl std::str::FromStr for InvariantEvery {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "every" | "1" => Ok(InvariantEvery::Every),
            "never" | "off" | "0" => Ok(InvariantEvery::Never),
            n => n.parse::<u64>().map(InvariantEvery::N).map_err(|_| format!("bad --check-invariants {n}")),
        }
    }
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ReplaySummary {
    pub blocks: u64,
    pub user_txs: u64,
    pub validator_txs: u64,
    pub order_actions: u64,
    pub receipts_ok: u64,
    pub receipts_err: u64,
    pub invariant_checks: u64,
    pub violations: Vec<String>,
    pub final_root: Hash32,
    /// blake3 over (height_le8 || root) for every block.
    pub roots_digest: Hash32,
    pub wall_ms: u128,
    pub liquidation_fills: u64,
    pub bad_debt_events: u64,
}

pub struct ReplayOpts {
    pub invariants: InvariantEvery,
    pub progress_ms: u64,
    /// Stop after this many violations (0 = never stop).
    pub max_violations: usize,
}

impl Default for ReplayOpts {
    fn default() -> Self {
        ReplayOpts { invariants: InvariantEvery::Every, progress_ms: 0, max_violations: 100 }
    }
}

/// Replay a log; `roots_out` receives `"{height} {root_hex}\n"` lines.
pub fn replay<R: Read>(
    reader: R,
    factory: &AppFactory,
    opts: &ReplayOpts,
    mut roots_out: Option<&mut dyn Write>,
) -> anyhow::Result<ReplaySummary> {
    let mut log = LogReader::new(reader)?;
    let genesis = log.header().genesis.clone();
    let mut app = factory(&genesis, AppOptions::default());
    let mut s = ReplaySummary::default();
    let mut digest = blake3::Hasher::new();
    let t0 = Instant::now();
    let mut last = Instant::now();
    while let Some(rec) = log.next_block()? {
        let outcome = app.execute_block(&rec.context(), &rec.txs);
        anyhow::ensure!(outcome.height == rec.height, "app executed height {} for record {}", outcome.height, rec.height);
        s.blocks += 1;
        for t in &rec.txs {
            match t {
                Tx::User(_) => s.user_txs += 1,
                Tx::Validator(_) => s.validator_txs += 1,
            }
        }
        s.order_actions += outcome.order_actions;
        for r in &outcome.receipts {
            if r.result.is_ok() {
                s.receipts_ok += 1;
            } else {
                s.receipts_err += 1;
            }
        }
        let mut covered = BTreeSet::new();
        for e in &outcome.events {
            match e {
                Event::Fill(f) if f.liquidation.is_some() => s.liquidation_fills += 1,
                Event::BadDebtCovered { user, .. } => {
                    s.bad_debt_events += 1;
                    covered.insert(*user);
                }
                _ => {}
            }
        }
        let check = match opts.invariants {
            InvariantEvery::Never => false,
            InvariantEvery::Every => true,
            InvariantEvery::N(n) => n > 0 && rec.height % n == 0,
        };
        if check {
            s.invariant_checks += 1;
            if let Err(e) = app.check_invariants() {
                s.violations.push(format!("height {}: {e}", rec.height));
            }
            for a in app.negative_equity_accounts() {
                if !covered.contains(&a) {
                    s.violations.push(format!("height {}: negative equity {a} after liquidation pass", rec.height));
                }
            }
            if opts.max_violations > 0 && s.violations.len() >= opts.max_violations {
                anyhow::bail!("too many invariant violations; first: {}", s.violations[0]);
            }
        }
        digest.update(&rec.height.to_le_bytes());
        digest.update(&outcome.state_root.0);
        s.final_root = outcome.state_root;
        if let Some(w) = roots_out.as_mut() {
            writeln!(w, "{} {}", rec.height, outcome.state_root.to_hex())?;
        }
        if opts.progress_ms > 0 && last.elapsed().as_millis() as u64 >= opts.progress_ms {
            last = Instant::now();
            eprintln!(
                "replay: height {} order_actions {} ({:.0}/s) violations {}",
                rec.height,
                s.order_actions,
                s.order_actions as f64 / t0.elapsed().as_secs_f64().max(1e-9),
                s.violations.len()
            );
        }
    }
    let f = log.footer().ok_or_else(|| anyhow::anyhow!("log has no footer"))?;
    anyhow::ensure!(f.blocks == s.blocks, "footer says {} blocks, replayed {}", f.blocks, s.blocks);
    s.roots_digest = Hash32(*digest.finalize().as_bytes());
    s.wall_ms = t0.elapsed().as_millis();
    Ok(s)
}
