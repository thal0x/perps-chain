//! `sim load`: live load against node APIs over HTTP `/exchange`.
//!
//! A producer thread runs the agent population blind (no chain feedback),
//! signs in parallel (rayon) and queues ready-to-post JSON bodies ahead of
//! time; a tokio sender paces them open-loop at the requested rate over
//! keep-alive connections and records statuses and latencies.

use crate::agents::{AgentKind, Mode, PopParams, Population};
use crate::app::World;
use crate::genesis::SimKeys;
use crate::sign::{sign_all, SignCtx, Signer, Unsigned};
use interfaces::*;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(clap::Args, Clone, Debug)]
pub struct LoadArgs {
    /// Comma-separated API base URLs, e.g. http://127.0.0.1:3001,http://127.0.0.1:3002
    #[arg(long, value_delimiter = ',', required = true)]
    pub targets: Vec<String>,
    /// Order actions per second (orders + cancels + modifies inside signed actions).
    #[arg(long, default_value_t = 10_000)]
    pub rate: u64,
    /// Seconds of traffic.
    #[arg(long, default_value_t = 30)]
    pub duration: u64,
    /// bench (MM batches of 50 + IOC takers) | mixed (full agent mix, blind)
    #[arg(long, default_value = "bench")]
    pub mode: String,
    /// keys.json from `sim genesis`, or hlnode's accounts.json ([{address, secret}]).
    #[arg(long)]
    pub keys: Option<PathBuf>,
    /// genesis.json (markets, chain flags). Default: derived from --seed like `sim genesis`.
    #[arg(long)]
    pub genesis: Option<PathBuf>,
    #[arg(long, default_value_t = 42)]
    pub seed: u64,
    #[arg(long, default_value_t = 4)]
    pub validators: usize,
    #[arg(long, default_value_t = 1000)]
    pub accounts: usize,
    /// Max in-flight requests (each waits for its block's receipt).
    #[arg(long, default_value_t = 4096)]
    pub concurrency: usize,
    /// Seconds of traffic pre-signed before sending starts.
    #[arg(long, default_value_t = 3)]
    pub presign_s: u64,
    #[arg(long, default_value_t = 0)]
    pub invalid_ppm: u32,
    #[arg(long, default_value_t = 15)]
    pub timeout_s: u64,
    #[arg(long, default_value_t = 2)]
    pub sign_threads: usize,
    /// Write the JSON report here as well as stdout.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct LoadReport {
    pub mode: String,
    pub targets: usize,
    pub duration_s: f64,
    pub requested_rate: u64,
    pub submitted: u64,
    pub accepted: u64,
    pub err_status: u64,
    pub http_errors: u64,
    pub timeouts: u64,
    pub order_actions_submitted: u64,
    pub order_actions_accepted: u64,
    /// Per-entry errors inside "ok" order/cancel responses.
    pub entry_errors: u64,
    pub submitted_actions_per_s: f64,
    pub latency_ms: Percentiles,
    pub late_sends: u64,
    pub sample_errors: Vec<String>,
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct Percentiles {
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub max: f64,
    pub mean: f64,
}

pub fn percentiles(mut v: Vec<u64>) -> Percentiles {
    if v.is_empty() {
        return Percentiles::default();
    }
    v.sort_unstable();
    let at = |q: usize| v[((v.len() - 1) * q / 100).min(v.len() - 1)] as f64 / 1000.0;
    Percentiles {
        p50: at(50),
        p90: at(90),
        p99: at(99),
        max: *v.last().unwrap() as f64 / 1000.0,
        mean: v.iter().sum::<u64>() as f64 / v.len() as f64 / 1000.0,
    }
}

/// One pre-signed request.
struct Item {
    send_at_us: u64,
    target: usize,
    body: Vec<u8>,
    order_actions: u64,
}

fn load_keys(a: &LoadArgs) -> anyhow::Result<SimKeys> {
    let Some(p) = &a.keys else {
        return Ok(SimKeys::derive(a.seed, a.validators, a.accounts));
    };
    let bytes = std::fs::read(p)?;
    let v: serde_json::Value = serde_json::from_slice(&bytes)?;
    if v.is_array() {
        // hlnode accounts.json: [{address, secret}]
        let mut accounts = Vec::new();
        for e in v.as_array().unwrap() {
            let s = e["secret"].as_str().ok_or_else(|| anyhow::anyhow!("accounts.json entry without secret"))?;
            accounts.push(hlwire::Wallet::from_secret_hex(s).map_err(|e| anyhow::anyhow!(e))?);
        }
        let d = SimKeys::derive(a.seed, 0, accounts.len());
        Ok(SimKeys { seed: a.seed, validators: vec![], agents: d.agents, kinds: d.kinds, accounts })
    } else {
        crate::genesis::read_keys_file(p)
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

pub fn run(a: LoadArgs) -> anyhow::Result<()> {
    let r = run_load(&a)?;
    let j = serde_json::to_string_pretty(&r)?;
    println!("{j}");
    if let Some(p) = &a.out {
        std::fs::write(p, &j)?;
    }
    Ok(())
}

const TICK_MS: u64 = 50;

pub fn run_load(a: &LoadArgs) -> anyhow::Result<LoadReport> {
    let mode = match a.mode.as_str() {
        "bench" => Mode::Bench,
        "mixed" => Mode::Mixed,
        m => anyhow::bail!("unknown --mode {m}"),
    };
    let mut keys = load_keys(a)?;
    anyhow::ensure!(!keys.accounts.is_empty(), "no accounts");
    let genesis = match &a.genesis {
        Some(p) => serde_json::from_slice::<Genesis>(&std::fs::read(p)?)?,
        None => crate::genesis::genesis(a.seed, a.validators.max(1), keys.accounts.len(), &crate::default_markets()).0,
    };
    if mode == Mode::Bench {
        // 80% market makers, 20% takers
        let n = keys.accounts.len();
        let mm = (n * 4 / 5).max(1);
        keys.kinds = (0..n).map(|i| if i < mm || n == 1 { AgentKind::MarketMaker } else { AgentKind::Taker }).collect();
    }
    // Anchor the agents' reference walk at "now" and, when the node serves
    // `allMids`, at its current prices (the walk otherwise starts from
    // genesis prices at genesis time).
    let mut genesis = genesis;
    genesis.genesis_time_ms = now_ms();
    if let Ok(mids) = fetch_mids(&a.targets[0]) {
        for m in genesis.markets.iter_mut() {
            if let Some(px) = mids.get(&m.name) {
                m.initial_px = crate::px::round_px(m, px.0, false);
            }
        }
    }
    let keys = Arc::new(keys);
    let n_targets = a.targets.len();
    // requests per second ~ rate / (mean order actions per request)
    let reqs_per_s_est = (a.rate / if mode == Mode::Bench { 45 } else { 4 }).max(1) as usize;
    let (tx, rx) = crossbeam_channel::bounded::<Item>((reqs_per_s_est * (a.presign_s as usize + 2)).max(1024));
    let ticks = a.duration * 1000 / TICK_MS;
    let budget_per_tick = (a.rate * TICK_MS / 1000).max(1);
    let base_ms = now_ms();

    // ---- producer
    let producer = {
        let keys = keys.clone();
        let genesis = genesis.clone();
        let invalid_ppm = a.invalid_ppm;
        let seed = a.seed;
        let sign_threads = a.sign_threads;
        std::thread::spawn(move || -> anyhow::Result<()> {
            let pool = rayon::ThreadPoolBuilder::new().num_threads(sign_threads.max(1)).build()?;
            let mut pop = Population::new(
                &genesis,
                &keys,
                PopParams { seed, mode, invalid_ppm, skewed_validators: 0, validators: false },
            );
            let world = World::new(genesis.markets.len(), keys.accounts.len(), false);
            let cx = SignCtx { keys: &keys, chain_id: genesis.chain_id, is_mainnet: genesis.is_mainnet, want_body: true };
            for t in 0..ticks {
                let ts = base_ms + t * TICK_MS;
                let us: Vec<Unsigned> = pop.step(t + 1, ts, budget_per_tick, &world, &keys);
                let signed = pool.install(|| sign_all(&cx, &us));
                let n = signed.len().max(1) as u64;
                for (j, (s, u)) in signed.into_iter().zip(&us).enumerate() {
                    let acct = match u {
                        Unsigned::User { signer: Signer::Account(i) | Signer::Agent(i), .. } => *i,
                        _ => j,
                    };
                    let Some(body) = s.body else { continue };
                    let item = Item {
                        send_at_us: t * TICK_MS * 1000 + j as u64 * TICK_MS * 1000 / n,
                        target: acct % n_targets,
                        body,
                        order_actions: s.order_actions,
                    };
                    if tx.send(item).is_err() {
                        return Ok(());
                    }
                }
            }
            Ok(())
        })
    };

    // pre-sign lead
    let lead = Instant::now();
    let want = reqs_per_s_est * a.presign_s as usize;
    while rx.len() < want && !producer.is_finished() && lead.elapsed() < Duration::from_secs(a.presign_s * 10 + 5) {
        std::thread::sleep(Duration::from_millis(20));
    }

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(2).build()?;
    let report = rt.block_on(send_all(a, rx))?;
    producer.join().map_err(|_| anyhow::anyhow!("producer panicked"))??;
    Ok(report)
}

/// `POST /info {"type":"allMids"}` -> name -> px.
pub fn fetch_mids(target: &str) -> anyhow::Result<std::collections::BTreeMap<String, Px>> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let c = reqwest::Client::builder().timeout(Duration::from_secs(3)).build()?;
        let v: serde_json::Value = c
            .post(format!("{}/info", target.trim_end_matches('/')))
            .json(&serde_json::json!({"type": "allMids"}))
            .send()
            .await?
            .json()
            .await?;
        let mut out = std::collections::BTreeMap::new();
        for (k, v) in v.as_object().ok_or_else(|| anyhow::anyhow!("allMids: not an object"))? {
            if let Some(px) = v.as_str().and_then(|s| hlwire::parse_px(s).ok()) {
                out.insert(k.clone(), px);
            }
        }
        Ok(out)
    })
}

#[derive(Default)]
struct Counters {
    submitted: AtomicU64,
    accepted: AtomicU64,
    err_status: AtomicU64,
    http_errors: AtomicU64,
    timeouts: AtomicU64,
    oa_sub: AtomicU64,
    oa_ok: AtomicU64,
    entry_errors: AtomicU64,
    late: AtomicU64,
}

fn count_entry_errors(v: &serde_json::Value) -> u64 {
    v.pointer("/response/data/statuses")
        .and_then(|s| s.as_array())
        .map(|a| a.iter().filter(|x| x.get("error").is_some() || x.as_str().map(|s| s != "success").unwrap_or(false)).count() as u64)
        .unwrap_or(0)
}

async fn send_all(a: &LoadArgs, rx: crossbeam_channel::Receiver<Item>) -> anyhow::Result<LoadReport> {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(a.concurrency)
        .timeout(Duration::from_secs(a.timeout_s))
        .tcp_nodelay(true)
        .build()?;
    let urls: Vec<Arc<str>> = a.targets.iter().map(|t| Arc::from(format!("{}/exchange", t.trim_end_matches('/')))).collect();
    let sem = Arc::new(tokio::sync::Semaphore::new(a.concurrency.max(1)));
    let c = Arc::new(Counters::default());
    let lat = Arc::new(Mutex::new(Vec::<u64>::new()));
    let samples = Arc::new(Mutex::new(Vec::<String>::new()));
    let start = Instant::now();
    loop {
        let item = match rx.try_recv() {
            Ok(i) => i,
            Err(crossbeam_channel::TryRecvError::Empty) => {
                tokio::time::sleep(Duration::from_millis(1)).await;
                continue;
            }
            Err(crossbeam_channel::TryRecvError::Disconnected) => break,
        };
        let due = Duration::from_micros(item.send_at_us);
        let el = start.elapsed();
        if due > el {
            tokio::time::sleep(due - el).await;
        } else if el - due > Duration::from_millis(100) {
            c.late.fetch_add(1, Relaxed);
        }
        let permit = sem.clone().acquire_owned().await?;
        let (client, url, c, lat, samples) = (client.clone(), urls[item.target].clone(), c.clone(), lat.clone(), samples.clone());
        c.submitted.fetch_add(1, Relaxed);
        c.oa_sub.fetch_add(item.order_actions, Relaxed);
        tokio::spawn(async move {
            let t = Instant::now();
            let res = client.post(&*url).header("content-type", "application/json").body(item.body).send().await;
            match res {
                Ok(resp) => {
                    let st = resp.status();
                    let bytes = resp.bytes().await.unwrap_or_default();
                    lat.lock().unwrap().push(t.elapsed().as_micros() as u64);
                    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
                    if st.is_success() && v.get("status").and_then(|s| s.as_str()) == Some("ok") {
                        c.accepted.fetch_add(1, Relaxed);
                        c.oa_ok.fetch_add(item.order_actions, Relaxed);
                        c.entry_errors.fetch_add(count_entry_errors(&v), Relaxed);
                    } else {
                        if st.is_success() {
                            c.err_status.fetch_add(1, Relaxed);
                        } else {
                            c.http_errors.fetch_add(1, Relaxed);
                        }
                        let mut s = samples.lock().unwrap();
                        if s.len() < 10 {
                            s.push(format!("{st}: {}", String::from_utf8_lossy(&bytes[..bytes.len().min(300)])));
                        }
                    }
                }
                Err(e) => {
                    if e.is_timeout() {
                        c.timeouts.fetch_add(1, Relaxed);
                    } else {
                        c.http_errors.fetch_add(1, Relaxed);
                    }
                    let mut s = samples.lock().unwrap();
                    if s.len() < 10 {
                        s.push(e.to_string());
                    }
                }
            }
            drop(permit);
        });
    }
    let send_secs = start.elapsed().as_secs_f64();
    // wait for in-flight requests
    let _all = sem.acquire_many(a.concurrency.max(1) as u32).await?;
    let l = std::mem::take(&mut *lat.lock().unwrap());
    let sample_errors = std::mem::take(&mut *samples.lock().unwrap());
    let g = |x: &AtomicU64| x.load(Relaxed);
    Ok(LoadReport {
        mode: a.mode.clone(),
        targets: a.targets.len(),
        duration_s: send_secs,
        requested_rate: a.rate,
        submitted: g(&c.submitted),
        accepted: g(&c.accepted),
        err_status: g(&c.err_status),
        http_errors: g(&c.http_errors),
        timeouts: g(&c.timeouts),
        order_actions_submitted: g(&c.oa_sub),
        order_actions_accepted: g(&c.oa_ok),
        entry_errors: g(&c.entry_errors),
        submitted_actions_per_s: g(&c.oa_sub) as f64 / send_secs.max(1e-9),
        latency_ms: percentiles(l),
        late_sends: g(&c.late),
        sample_errors,
    })
}
