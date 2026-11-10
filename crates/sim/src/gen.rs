//! `sim gen`: run the agent population against an embedded application and
//! write the block log.

use crate::agents::{Mode, PopParams, PopStats, Population};
use crate::app::{AppFactory, AppOptions, World};
use crate::genesis::{genesis_hash, GenesisOpts, SimKeys};
use crate::log::{BlockRecord, GenParams, LogFooter, LogHeader, LogWriter};
use crate::sign::{sign_all, SignCtx};
use interfaces::*;
use std::io::Write;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct GenConfig {
    pub seed: u64,
    pub actions: u64,
    pub sim_ms: u64,
    pub block_ms: u64,
    pub validators: usize,
    pub accounts: usize,
    pub invalid_ppm: u32,
    pub skewed_validators: usize,
    pub markets: Vec<MarketSpec>,
    pub genesis_opts: GenesisOpts,
    /// Print progress to stderr every N wall-clock ms (0 = never).
    pub progress_ms: u64,
}

impl GenConfig {
    pub fn small(seed: u64) -> Self {
        GenConfig {
            seed,
            actions: 5_000,
            sim_ms: 60_000,
            block_ms: 1_000,
            validators: 4,
            accounts: 60,
            invalid_ppm: 20_000,
            skewed_validators: 1,
            markets: crate::genesis::default_markets(),
            genesis_opts: GenesisOpts::default(),
            progress_ms: 0,
        }
    }
    pub fn blocks(&self) -> u64 {
        (self.sim_ms / self.block_ms.max(1)).max(1)
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct GenSummary {
    pub footer: LogFooter,
    pub final_root: Hash32,
    pub roots_digest: Hash32,
    pub wall_ms: u128,
    pub actions_per_s: u64,
    pub stats: PopStats,
    pub receipts_ok: u64,
    pub receipts_err: u64,
    pub fills: u64,
    pub liquidation_fills: u64,
    pub bad_debt_events: u64,
    /// Result histogram: tx-level errors and per-entry statuses (truncated).
    pub results: Vec<(String, u64)>,
    /// Wall ms spent in: generate, sign, execute, refresh, log back-pressure.
    pub phase_ms: [u64; 5],
}

/// Histogram key for a message: digits collapsed so ids don't explode it.
fn norm(s: &str) -> String {
    let mut o = String::new();
    let b: Vec<char> = s.chars().take(120).collect();
    let mut i = 0;
    while i < b.len() {
        if b[i] == '0' && i + 1 < b.len() && b[i + 1] == 'x' {
            o.push_str("0x..");
            i += 2;
            while i < b.len() && b[i].is_ascii_hexdigit() {
                i += 1;
            }
        } else if b[i].is_ascii_digit() {
            o.push('#');
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == '.') {
                i += 1;
            }
        } else {
            o.push(b[i]);
            i += 1;
        }
    }
    o
}

pub fn tally_receipt(h: &mut std::collections::BTreeMap<String, u64>, r: &TxReceipt) {
    let mut add = |k: String| *h.entry(k).or_default() += 1;
    match &r.result {
        Err(e) => add(format!("tx err: {}", norm(e))),
        Ok(ActionResponse::Default) => add("tx ok: default".into()),
        Ok(ActionResponse::Order(st)) => {
            for s in st {
                add(match s {
                    OrderStatusResp::Resting { .. } => "order: resting".into(),
                    OrderStatusResp::Filled { .. } => "order: filled".into(),
                    OrderStatusResp::WaitingForTrigger { .. } => "order: waitingForTrigger".into(),
                    OrderStatusResp::WaitingForFill { .. } => "order: waitingForFill".into(),
                    OrderStatusResp::Error(e) => format!("order err: {}", norm(e)),
                })
            }
        }
        Ok(ActionResponse::Cancel(st)) => {
            for s in st {
                add(match s {
                    SimpleStatus::Success => "cancel: success".into(),
                    SimpleStatus::Error(e) => format!("cancel err: {}", norm(e)),
                })
            }
        }
    }
}

/// Generate a log. `on_root(height, root)` is called per executed block
/// (e.g. to write the gen-time roots file).
pub fn generate<W: Write + Send>(
    cfg: &GenConfig,
    factory: &AppFactory,
    out: W,
    mut on_root: impl FnMut(Height, Hash32),
) -> anyhow::Result<GenSummary> {
    let (genesis, keys) = crate::genesis::genesis_with(cfg.seed, cfg.validators, cfg.accounts, &cfg.markets, &cfg.genesis_opts);
    let blocks = cfg.blocks();
    let params = GenParams {
        seed: cfg.seed,
        actions: cfg.actions,
        blocks,
        block_ms: cfg.block_ms,
        validators: cfg.validators as u32,
        accounts: cfg.accounts as u32,
        invalid_ppm: cfg.invalid_ppm,
        skewed_validators: cfg.skewed_validators as u32,
        markets: genesis.markets.iter().map(|m| m.name.clone()).collect(),
    };
    let header = LogHeader {
        genesis_hash: genesis_hash(&genesis),
        seed: cfg.seed,
        params,
        genesis: genesis.clone(),
        generator: format!("sim {}", env!("CARGO_PKG_VERSION")),
    };
    let mut app = factory(&genesis, AppOptions::default());
    let mut pop = Population::new(
        &genesis,
        &keys,
        PopParams {
            seed: cfg.seed,
            mode: Mode::Mixed,
            invalid_ppm: cfg.invalid_ppm,
            skewed_validators: cfg.skewed_validators,
            validators: true,
        },
    );
    let feedback = app.has_feedback();
    let mut world = World::new(genesis.markets.len(), cfg.accounts, feedback);
    let max_actions_per_block = (genesis.params.max_block_order_actions as u64 * 9 / 10).max(1);

    // Log writing (borsh + zstd) runs on its own thread.
    let (tx, rx) = crossbeam_channel::bounded::<(BlockRecord, u64, u64)>(8);
    let t0 = Instant::now();
    let mut roots = blake3::Hasher::new();
    let mut final_root = Hash32::ZERO;
    let (mut ok, mut err) = (0u64, 0u64);
    let mut hist = std::collections::BTreeMap::<String, u64>::new();
    let mut phase = [0u64; 5];
    let (mut fills, mut liq_fills, mut bad_debt) = (0u64, 0u64, 0u64);
    let footer = std::thread::scope(|s| -> anyhow::Result<LogFooter> {
        let writer = s.spawn(move || -> anyhow::Result<LogFooter> {
            let mut w = LogWriter::new(out, &header)?;
            for (rec, oa, act) in rx {
                w.append_counted(&rec, oa, act)?;
            }
            w.finish()
        });
        let cx = SignCtx { keys: &keys, chain_id: genesis.chain_id, is_mainnet: genesis.is_mainnet, want_body: false };
        let mut generated = 0u64;
        let mut last_report = Instant::now();
        let n_val = genesis.validators.validators.len() as u64;
        // Pipeline: block h executes (and its world delta is collected) on a
        // scoped thread while block h+1 is generated from the world after
        // h-1 and signed. The schedule is fixed, so output is deterministic.
        let mut inflight: Option<(BlockRecord, Interest, u64, u64)> = None;
        let mut h = 1u64;
        while h <= blocks + 1 {
            let app_ref = &mut app;
            let (exec_res, next) = std::thread::scope(|s2| {
                let jh = inflight.take().map(|(rec, it, oa, act)| {
                    s2.spawn(move || {
                        let tp = Instant::now();
                        let outcome = app_ref.execute_block(&rec.context(), &rec.txs);
                        let te = tp.elapsed().as_micros() as u64;
                        let tp = Instant::now();
                        let delta = World::collect(app_ref.query(), feedback, &it.accounts, &it.orders);
                        (rec, it, oa, act, outcome, delta, te, tp.elapsed().as_micros() as u64)
                    })
                });
                let next = if h <= blocks {
                    let ts = genesis.genesis_time_ms + h * cfg.block_ms;
                    let target = (cfg.actions as u128 * h as u128 / blocks as u128) as u64;
                    let budget = target.saturating_sub(generated).min(max_actions_per_block);
                    let tp = Instant::now();
                    let unsigned = pop.step(h, ts, budget, &world, &keys);
                    let (accounts, orders) = pop.interest();
                    phase[0] += tp.elapsed().as_micros() as u64;
                    let tp = Instant::now();
                    let signed = sign_all(&cx, &unsigned);
                    phase[1] += tp.elapsed().as_micros() as u64;
                    let acted: Vec<usize> = orders.iter().map(|o| o.0).collect();
                    Some((ts, signed, Interest { accounts, orders, acted }))
                } else {
                    None
                };
                (jh.map(|j| j.join().expect("executor thread panicked")), next)
            });
            if let Some((rec, _it, oa, act, outcome, delta, te, tr)) = exec_res {
                phase[2] += te;
                phase[3] += tr;
                for r in &outcome.receipts {
                    if r.result.is_ok() {
                        ok += 1;
                    } else {
                        err += 1;
                    }
                    tally_receipt(&mut hist, r);
                }
                for e in &outcome.events {
                    match e {
                        Event::Fill(f) => {
                            fills += 1;
                            if f.liquidation.is_some() {
                                liq_fills += 1;
                            }
                        }
                        Event::BadDebtCovered { .. } => bad_debt += 1,
                        _ => {}
                    }
                }
                final_root = outcome.state_root;
                roots.update(&rec.height.to_le_bytes());
                roots.update(&outcome.state_root.0);
                on_root(rec.height, outcome.state_root);
                world.apply(delta);
                // accounts that acted in the block generated meanwhile keep
                // their local view until that block's own refresh
                let skip: &[usize] = next.as_ref().map(|n| &n.2.acted[..]).unwrap_or(&[]);
                pop.after_block(&world, skip);
                let tp = Instant::now();
                let sent = tx.send((rec, oa, act));
                phase[4] += tp.elapsed().as_micros() as u64;
                if sent.is_err() {
                    break; // writer failed; its error is reported below
                }
            }
            if let Some((ts, signed, it)) = next {
                let mut txs = Vec::with_capacity(signed.len());
                let (mut oa, mut act) = (0u64, 0u64);
                for sgn in signed {
                    if let Tx::User(_) = &sgn.tx {
                        generated += sgn.order_actions.max(1);
                        oa += sgn.order_actions;
                        act += sgn.order_actions.max(1);
                    }
                    if let Some((a, signer)) = sgn.known {
                        app.prime_recovered(sgn.tx.hash(), a, signer);
                    }
                    txs.push(sgn.tx);
                }
                let rec = BlockRecord {
                    height: h,
                    timestamp_ms: ts,
                    proposer: genesis.validators.validators[(h % n_val) as usize].address,
                    txs,
                };
                inflight = Some((rec, it, oa, act));
            }
            if cfg.progress_ms > 0 && last_report.elapsed().as_millis() as u64 >= cfg.progress_ms {
                last_report = Instant::now();
                let secs = t0.elapsed().as_secs_f64().max(1e-9);
                eprintln!(
                    "gen: height {h}/{blocks} actions {generated}/{} ({:.0}/s) receipts ok {ok} err {err}",
                    cfg.actions,
                    generated as f64 / secs
                );
            }
            h += 1;
        }
        drop(tx);
        writer.join().map_err(|_| anyhow::anyhow!("log writer panicked"))?
    })?;
    let wall_ms = t0.elapsed().as_millis();
    Ok(GenSummary {
        actions_per_s: (footer.actions as u128 * 1000 / wall_ms.max(1)) as u64,
        footer,
        final_root,
        roots_digest: Hash32(*roots.finalize().as_bytes()),
        wall_ms,
        stats: pop.stats(),
        receipts_ok: ok,
        receipts_err: err,
        fills,
        liquidation_fills: liq_fills,
        bad_debt_events: bad_debt,
        results: hist.into_iter().collect(),
        phase_ms: phase.map(|x| x / 1000),
    })
}

struct Interest {
    accounts: Vec<(usize, Address)>,
    orders: Vec<(usize, Address)>,
    acted: Vec<usize>,
}

/// Keys behind a config (same derivation as [`generate`]).
pub fn keys_for(cfg: &GenConfig) -> (Genesis, SimKeys) {
    crate::genesis::genesis_with(cfg.seed, cfg.validators, cfg.accounts, &cfg.markets, &cfg.genesis_opts)
}
