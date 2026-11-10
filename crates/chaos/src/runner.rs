//! The `hlchaos run` loop: start the testnet, inject seeded fault episodes,
//! heal, measure resumption, keep the safety checker running, write the
//! report.

use crate::checker::{SafetyChecker, Violation, ViolationKind};
use crate::cluster::{Cluster, CONSOLE_LOG};
use crate::load::{self, BuiltinLoad, LoadStats};
use crate::report::{EpisodeRecord, HealRecord, Report};
use crate::schedule::{chaos_json, faulty_set, Action, Episode, EpisodeKind, NetPlan, Scheduler, SchedulerConfig};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Set by the signal handler (SIGINT / SIGTERM / SIGHUP).
pub static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

pub fn install_signal_handlers() {
    unsafe {
        for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(s, on_signal as *const () as libc::sighandler_t);
        }
    }
}

#[derive(Clone, Debug)]
pub struct RunConfig {
    pub validators: usize,
    pub duration_ms: u64,
    pub max_faulty: usize,
    pub seed: u64,
    pub hlnode: PathBuf,
    pub workdir: PathBuf,
    pub load_rate: f64,
    pub sim: Option<PathBuf>,
    pub sim_args: String,
    pub report: Option<PathBuf>,
    pub episode_min_ms: u64,
    pub episode_max_ms: u64,
    pub observe_ms: u64,
    pub resume_limit_ms: u64,
    pub stall_window_ms: u64,
    pub split_grace_ms: u64,
    pub startup_timeout_ms: u64,
    pub kinds: Vec<EpisodeKind>,
    pub node_args: Vec<String>,
    pub accounts: usize,
    pub allow_gaps: bool,
    pub catchup_slack: u64,
    pub progress_every_ms: u64,
    pub flush_every_ms: u64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            validators: 7,
            duration_ms: 180_000,
            max_faulty: 2,
            seed: 1,
            hlnode: PathBuf::from("hlnode"),
            workdir: PathBuf::from("chaos-work"),
            load_rate: 5.0,
            sim: None,
            sim_args: "load --rps {rate} --target {target}".into(),
            report: None,
            episode_min_ms: 20_000,
            episode_max_ms: 120_000,
            observe_ms: 20_000,
            resume_limit_ms: 10_000,
            stall_window_ms: 30_000,
            split_grace_ms: 5_000,
            startup_timeout_ms: 90_000,
            kinds: EpisodeKind::ALL.to_vec(),
            node_args: vec![],
            accounts: 8,
            allow_gaps: false,
            catchup_slack: 3,
            progress_every_ms: 30_000,
            flush_every_ms: 30_000,
        }
    }
}

struct EpCtx {
    halting: bool,
    split_start: Instant,
    split_baseline: Option<u64>,
    split_flagged: bool,
    stalls: u64,
}

struct Runner {
    cfg: RunConfig,
    cluster: Cluster,
    checker: SafetyChecker,
    report: Report,
    report_path: PathBuf,
    start: Instant,
    killed: BTreeSet<usize>,
    plan: NetPlan,
    addrs: Vec<String>,
    desired: Vec<Value>,
    applied: Vec<Option<Value>>,
    last_apply_try: Vec<Option<Instant>>,
    max_height: u64,
    last_progress: Instant,
    stall_open: bool,
    ep: Option<EpCtx>,
    ep_label: String,
    last_check: Instant,
    last_line: Instant,
    last_flush: Instant,
    load_stats: Option<Arc<LoadStats>>,
    load_stop: Arc<AtomicBool>,
    sim_child: Option<Child>,
    seen_violations: u64,
    interrupted: bool,
}

fn healed() -> Value {
    serde_json::json!({})
}

impl Runner {
    fn t(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn say(&self, msg: impl AsRef<str>) {
        println!("[{:>9.1}s] {}", self.t(), msg.as_ref());
    }

    fn heights(&self) -> Vec<u64> {
        (0..self.cluster.n()).map(|i| self.cluster.obs(i).committed_height).collect()
    }

    fn observed_max(&self) -> u64 {
        let m = self.heights().into_iter().max().unwrap_or(0);
        m.max(self.checker.stats.max_committed)
    }

    fn apply_pending(&mut self, force: bool) {
        for i in 0..self.cluster.n() {
            if !self.cluster.is_running(i) || self.applied[i].as_ref() == Some(&self.desired[i]) {
                continue;
            }
            if !force {
                if let Some(t) = self.last_apply_try[i] {
                    if t.elapsed() < Duration::from_millis(250) {
                        continue;
                    }
                }
            }
            self.last_apply_try[i] = Some(Instant::now());
            let body = self.desired[i].to_string();
            if self.cluster.post_chaos(i, &body).is_ok() {
                self.applied[i] = Some(self.desired[i].clone());
            }
        }
    }

    fn set_plan(&mut self, plan: NetPlan, ep_id: u64) {
        let seed = self.cfg.seed ^ ep_id.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        for i in 0..self.cluster.n() {
            self.desired[i] = chaos_json(&plan, i, &self.addrs, seed);
        }
        self.plan = plan;
        self.apply_pending(true);
    }

    fn tick(&mut self) {
        if STOP.load(Ordering::SeqCst) {
            self.interrupted = true;
        }
        // unexpected exits
        for (i, st) in self.cluster.poll_exits() {
            self.report.unexpected_exits += 1;
            self.say(format!("!! node{i} exited unexpectedly ({st}); restarting it (see {}/{CONSOLE_LOG})", self.cluster.nodes[i].home.display()));
            self.checker.on_killed(i);
            self.applied[i] = None;
            if !self.killed.contains(&i) {
                if let Err(e) = self.cluster.start(i) {
                    self.report.errors.push(format!("restart node{i} after crash: {e}"));
                }
            }
        }
        self.apply_pending(false);
        // heights / progress
        let h = self.observed_max();
        if h > self.max_height {
            self.max_height = h;
            self.last_progress = Instant::now();
            self.stall_open = false;
        }
        let halting = self.ep.as_ref().map(|e| e.halting).unwrap_or(false);
        if !halting && !self.stall_open && self.last_progress.elapsed() > Duration::from_millis(self.cfg.stall_window_ms) {
            self.stall_open = true;
            self.report.progress_stalls += 1;
            if let Some(e) = self.ep.as_mut() {
                e.stalls += 1;
            }
            self.say(format!(
                "!! STALL: no commit for {} ms during a non-halting phase ({}); heights {:?}",
                self.last_progress.elapsed().as_millis(),
                self.ep_label,
                self.heights()
            ));
        }
        // a quorumless split must not make progress after a grace period
        let mut split_violation = None;
        if let Some(e) = self.ep.as_mut() {
            if e.halting && e.split_start.elapsed() > Duration::from_millis(self.cfg.split_grace_ms) {
                match e.split_baseline {
                    None => e.split_baseline = Some(self.max_height),
                    Some(b) if self.max_height > b && !e.split_flagged => {
                        e.split_flagged = true;
                        split_violation = Some((b, self.max_height));
                    }
                    _ => {}
                }
            }
        }
        if let Some((b, now_h)) = split_violation {
            self.checker.record(Violation {
                kind: ViolationKind::ProgressWithoutQuorum,
                node: usize::MAX,
                height: now_h,
                detail: format!("max committed height advanced {b} -> {now_h} during a quorumless split ({})", self.ep_label),
            });
        }
        if self.last_check.elapsed() >= Duration::from_millis(500) {
            self.last_check = Instant::now();
            self.checker.poll();
        }
        if self.checker.violation_count > self.seen_violations {
            for v in self.checker.violations.iter().skip(self.seen_violations as usize) {
                println!("[{:>9.1}s] !! SAFETY VIOLATION {:?}: {}", self.start.elapsed().as_secs_f64(), v.kind, v.detail);
            }
            self.seen_violations = self.checker.violation_count;
        }
        if self.last_line.elapsed() >= Duration::from_millis(self.cfg.progress_every_ms) {
            self.last_line = Instant::now();
            let load = self.load_stats.as_ref().map(|s| s.snapshot()).unwrap_or_default();
            self.say(format!(
                "progress: {} | heights {:?} max {} | episodes {} heals {} max_resume {} ms | violations {} stalls {} | load sent {} ok {}",
                self.ep_label,
                self.heights(),
                self.max_height,
                self.report.episodes,
                self.report.heal_records.len(),
                self.report.heal_records.iter().filter_map(|h| h.resume_ms).max().unwrap_or(0),
                self.checker.violation_count,
                self.report.progress_stalls,
                load.get("sent").copied().unwrap_or(0),
                load.get("accepted").copied().unwrap_or(0),
            ));
        }
        if self.last_flush.elapsed() >= Duration::from_millis(self.cfg.flush_every_ms) {
            self.last_flush = Instant::now();
            self.flush(false);
        }
    }

    /// Tick until `deadline`; false if interrupted.
    fn wait_until(&mut self, deadline: Instant) -> bool {
        loop {
            self.tick();
            if self.interrupted {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(100)));
        }
    }

    fn fill_report(&mut self) {
        self.report.duration_s = self.start.elapsed().as_secs();
        self.report.safety_violations = self.checker.violation_count;
        self.report.violations = self.checker.violations.clone();
        self.report.violations_by_kind = self.checker.by_kind.clone();
        self.report.checker = self.checker.stats.clone();
        self.report.final_height = self.max_height;
        if let Some(s) = &self.load_stats {
            self.report.load = s.snapshot();
        }
        self.report.interrupted = self.interrupted;
        self.report.finalize_stats();
    }

    fn flush(&mut self, complete: bool) {
        self.fill_report();
        self.report.complete = complete;
        if !complete {
            self.report.pass = false;
            self.report.summary = format!("in progress: {} s, {} episodes, {} violations", self.report.duration_s, self.report.episodes, self.report.safety_violations);
        }
        if let Err(e) = self.report.write(&self.report_path) {
            eprintln!("cannot write report {}: {e}", self.report_path.display());
        }
    }

    fn apply_step(&mut self, ep: &Episode, a: &Action) {
        // runtime re-check of the invariant (the scheduler is property-tested)
        let mut killed = self.killed.clone();
        let mut plan = self.plan.clone();
        match a {
            Action::Kill { node } => {
                killed.insert(*node);
            }
            Action::Restart { node } => {
                killed.remove(node);
            }
            Action::Net { plan: p } => plan = p.clone(),
        }
        let ok = match faulty_set(self.cluster.n(), &killed, &plan) {
            None => ep.halting,
            Some(f) => f.len() <= self.cfg.max_faulty || (ep.halting && f.is_empty()),
        };
        if !ok {
            let msg = format!("episode {} step {:?} would break the max-faulty invariant; skipped", ep.id, a);
            self.say(format!("!! {msg}"));
            self.report.errors.push(msg);
            return;
        }
        match a {
            Action::Kill { node } => {
                self.cluster.kill(*node);
                self.checker.on_killed(*node);
                self.killed.insert(*node);
                self.applied[*node] = None;
                self.report.kills += 1;
                self.say(format!("   kill -9 node{node}"));
            }
            Action::Restart { node } => {
                self.restart(*node);
            }
            Action::Net { plan } => {
                self.set_plan(plan.clone(), ep.id);
                self.say(format!("   net plan applied ({} nodes configured)", self.applied.iter().filter(|a| a.is_some()).count()));
            }
        }
    }

    fn restart(&mut self, node: usize) {
        match self.cluster.start(node) {
            Ok(()) => {
                self.killed.remove(&node);
                self.applied[node] = None;
                self.report.restarts += 1;
                self.say(format!("   restart node{node}"));
            }
            Err(e) => {
                self.report.errors.push(format!("restart node{node}: {e}"));
                self.say(format!("!! restart node{node} failed: {e}"));
            }
        }
    }

    fn run_episode(&mut self, ep: Episode) {
        let t0 = Instant::now();
        let start_s = self.t();
        self.report.episodes += 1;
        *self.report.episodes_by_kind.entry(ep.kind.name().to_string()).or_default() += 1;
        if ep.has_partition() {
            self.report.partitions += 1;
        }
        if ep.halting {
            self.report.splits += 1;
        }
        if ep.steps.iter().any(|s| matches!(&s.action, Action::Net { plan } if !plan.lossy.is_empty())) {
            self.report.lossy_episodes += 1;
        }
        self.ep_label = format!("ep {} {}", ep.id, ep.kind.name());
        self.say(format!("== episode {} [{}] {} ms: {}", ep.id, ep.kind.name(), ep.duration_ms, ep.desc));
        let height_start = self.max_height;
        self.ep = Some(EpCtx { halting: ep.halting, split_start: t0, split_baseline: None, split_flagged: false, stalls: 0 });
        for s in &ep.steps {
            if !self.wait_until(t0 + Duration::from_millis(s.at_ms)) {
                break;
            }
            self.apply_step(&ep, &s.action);
            if let Some(e) = self.ep.as_mut() {
                if ep.halting {
                    e.split_start = Instant::now();
                }
            }
        }
        if !self.interrupted {
            self.wait_until(t0 + Duration::from_millis(ep.duration_ms));
        }
        let height_end = self.max_height;
        let ep_stalls = self.ep.as_ref().map(|e| e.stalls).unwrap_or(0);
        // ---- heal ----
        let to_restart: Vec<usize> = self.killed.iter().copied().collect();
        for i in to_restart {
            self.restart(i);
        }
        self.set_plan(NetPlan::default(), ep.id);
        let t_heal = Instant::now();
        let heal_at = self.t();
        let base = self.observed_max().max(self.max_height);
        self.ep = None;
        self.ep_label = format!("observe after ep {}", ep.id);
        self.last_progress = t_heal; // stall window restarts at the heal
        self.stall_open = false;
        self.say(format!("   heal (base height {base})"));
        let cap = Duration::from_millis(self.cfg.observe_ms.max(self.cfg.resume_limit_ms * 2));
        let mut resume_ms = None;
        let mut catchup_ms = None;
        while !self.interrupted && t_heal.elapsed() < cap {
            self.tick();
            if resume_ms.is_none() && self.observed_max() >= base + 2 {
                resume_ms = Some(t_heal.elapsed().as_millis() as u64);
            }
            if catchup_ms.is_none() {
                let max = self.observed_max();
                let all = (0..self.cluster.n()).all(|i| {
                    let o = self.cluster.obs(i);
                    o.up && o.committed_height + self.cfg.catchup_slack >= max
                });
                if all && resume_ms.is_some() {
                    catchup_ms = Some(t_heal.elapsed().as_millis() as u64);
                }
            }
            if resume_ms.is_some() && catchup_ms.is_some() && t_heal.elapsed() >= Duration::from_millis(self.cfg.observe_ms) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let ok = resume_ms.map(|r| r <= self.cfg.resume_limit_ms).unwrap_or(false);
        self.say(format!(
            "   resume {} (limit {} ms){}, catch-up {}",
            resume_ms.map(|r| format!("{r} ms")).unwrap_or_else(|| "NEVER".into()),
            self.cfg.resume_limit_ms,
            if ok { "" } else { " !! LIVENESS FAILURE" },
            catchup_ms.map(|r| format!("{r} ms")).unwrap_or_else(|| "incomplete".into()),
        ));
        if !self.interrupted {
            self.report.heal_records.push(HealRecord {
                episode: ep.id,
                kind: ep.kind.name().to_string(),
                halting: ep.halting,
                at_s: heal_at,
                base_height: base,
                resume_ms,
                catchup_ms,
            });
        }
        self.report.episode_records.push(EpisodeRecord {
            id: ep.id,
            kind: ep.kind.name().to_string(),
            desc: ep.desc.clone(),
            start_s,
            duration_ms: ep.duration_ms,
            halting: ep.halting,
            height_start,
            height_end,
            stalls: ep_stalls,
        });
        self.flush(false);
    }
}

fn unix_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Run the whole thing. Returns the final report (also written to disk).
pub fn run(cfg: RunConfig) -> Result<Report> {
    if cfg.validators < 4 {
        bail!("need at least 4 validators");
    }
    let run_dir = cfg.workdir.join(format!("run-{}-{}", cfg.seed, unix_s()));
    std::fs::create_dir_all(&run_dir).with_context(|| format!("create {}", run_dir.display()))?;
    let report_path = cfg.report.clone().unwrap_or_else(|| run_dir.join("report.json"));
    let f = cfg.validators.saturating_sub(1) / 3;
    if cfg.max_faulty > f {
        eprintln!("warning: --max-faulty {} > f = {f} for {} validators; non-halting episodes are clamped to {f}", cfg.max_faulty, cfg.validators);
    }
    println!("hlchaos: run dir {}  report {}", run_dir.display(), report_path.display());
    let cluster = Cluster::generate(&cfg.hlnode, &run_dir, cfg.validators, cfg.seed, cfg.accounts, cfg.node_args.clone())?;
    let homes: Vec<PathBuf> = cluster.nodes.iter().map(|n| n.home.clone()).collect();
    let checker = SafetyChecker::with_homes(&homes, &[CONSOLE_LOG], cfg.allow_gaps);
    let n = cfg.validators;
    let addrs = cluster.addresses();
    let mut sched = Scheduler::new(
        SchedulerConfig { n, max_faulty: cfg.max_faulty, min_ms: cfg.episode_min_ms, max_ms: cfg.episode_max_ms, kinds: cfg.kinds.clone() },
        cfg.seed,
    );
    let mut r = Runner {
        report: Report {
            validators: n,
            max_faulty: sched.budget(),
            seed: cfg.seed,
            resume_limit_ms: cfg.resume_limit_ms,
            ..Default::default()
        },
        report_path,
        start: Instant::now(),
        killed: BTreeSet::new(),
        plan: NetPlan::default(),
        desired: vec![healed(); n],
        applied: vec![None; n],
        last_apply_try: vec![None; n],
        addrs,
        max_height: 0,
        last_progress: Instant::now(),
        stall_open: false,
        ep: None,
        ep_label: "startup".into(),
        last_check: Instant::now(),
        last_line: Instant::now(),
        last_flush: Instant::now(),
        load_stats: None,
        load_stop: Arc::new(AtomicBool::new(false)),
        sim_child: None,
        seen_violations: 0,
        interrupted: false,
        checker,
        cluster,
        cfg,
    };
    let result = r.main_loop(&mut sched);
    if let Err(e) = &result {
        r.report.errors.push(format!("{e:#}"));
        r.say(format!("!! harness error: {e:#}"));
    }
    r.shutdown();
    Ok(r.report)
}

impl Runner {
    fn main_loop(&mut self, sched: &mut Scheduler) -> Result<()> {
        for i in 0..self.cluster.n() {
            self.cluster.start(i)?;
        }
        self.cluster.start_pollers(Duration::from_millis(100));
        self.say(format!("started {} validators; waiting for APIs", self.cluster.n()));
        let deadline = self.start + Duration::from_millis(self.cfg.startup_timeout_ms);
        loop {
            self.tick();
            if self.interrupted {
                return Ok(());
            }
            let up = (0..self.cluster.n()).filter(|i| self.cluster.obs(*i).up).count();
            if up == self.cluster.n() && self.max_height >= 3 {
                break;
            }
            if Instant::now() > deadline {
                bail!("startup timeout: {up}/{} APIs up, max committed height {}", self.cluster.n(), self.max_height);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // verify the chaos endpoint works on every node
        for i in 0..self.cluster.n() {
            self.cluster.post_chaos(i, "{}").with_context(|| format!("node{i}: POST /admin/chaos"))?;
            self.applied[i] = Some(healed());
        }
        self.say(format!("network live at height {}; starting load and episodes", self.max_height));
        self.start_load();
        let total = Duration::from_millis(self.cfg.duration_ms);
        loop {
            if self.interrupted {
                break;
            }
            let el = self.start.elapsed();
            if el >= total {
                break;
            }
            let remaining = (total - el).as_millis() as u64;
            let obs = self.cfg.observe_ms;
            if remaining < obs + 1_000 {
                break;
            }
            let max_ep = remaining - obs;
            let ep = sched.next_episode(Some(max_ep));
            self.run_episode(ep);
        }
        if !self.interrupted {
            let end = self.start + total;
            self.wait_until(end.max(Instant::now()));
        }
        Ok(())
    }

    fn start_load(&mut self) {
        if self.cfg.load_rate <= 0.0 {
            return;
        }
        let apis: Vec<String> = self.cluster.nodes.iter().map(|n| n.api.clone()).collect();
        if let Some(sim) = &self.cfg.sim {
            match load::spawn_sim(sim, &self.cfg.sim_args, self.cfg.load_rate, &apis, &self.cluster.net_dir.join("sim-load.log")) {
                Ok(c) => {
                    self.say(format!("load: external {} ({} tx/s)", sim.display(), self.cfg.load_rate));
                    self.sim_child = Some(c);
                    return;
                }
                Err(e) => self.say(format!("load: cannot start {}: {e}; using built-in generator", sim.display())),
            }
        }
        let wallets = load::read_accounts(&self.cluster.net_dir.join("accounts.json"));
        if wallets.is_empty() {
            self.say("load: no funded accounts in accounts.json; load disabled");
            return;
        }
        let is_mainnet = std::fs::read(&self.cluster.genesis)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|g| g.get("is_mainnet").and_then(|x| x.as_bool()))
            .unwrap_or(false);
        self.say(format!("load: built-in generator, {} accounts, {} tx/s", wallets.len(), self.cfg.load_rate));
        self.load_stats = Some(
            BuiltinLoad { apis, alive: self.cluster.alive.clone(), wallets, is_mainnet, rate: self.cfg.load_rate, seed: self.cfg.seed }
                .spawn(self.load_stop.clone()),
        );
    }

    fn shutdown(&mut self) {
        self.load_stop.store(true, Ordering::SeqCst);
        if let Some(mut c) = self.sim_child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.checker.poll();
        self.cluster.stop_all(Duration::from_secs(5));
        // final drain (nodes may flush on shutdown); torn tails are harmless
        self.checker.poll();
        self.fill_report();
        let r = &mut self.report;
        let mut reasons = vec![];
        if r.interrupted {
            reasons.push("interrupted".to_string());
        }
        if !r.errors.is_empty() {
            reasons.push(format!("{} harness errors", r.errors.len()));
        }
        if r.safety_violations > 0 {
            reasons.push(format!("{} safety violations", r.safety_violations));
        }
        if r.heals == 0 {
            reasons.push("no heals".into());
        }
        let never = r.heal_records.iter().filter(|h| h.resume_ms.is_none()).count();
        if never > 0 {
            reasons.push(format!("{never} heals never resumed"));
        }
        if r.max_resume_ms > r.resume_limit_ms {
            reasons.push(format!("max_resume_ms {} > {}", r.max_resume_ms, r.resume_limit_ms));
        }
        if r.progress_stalls > 0 {
            reasons.push(format!("{} progress stalls", r.progress_stalls));
        }
        if r.unexpected_exits > 0 {
            reasons.push(format!("{} unexpected node exits", r.unexpected_exits));
        }
        r.pass = reasons.is_empty();
        r.complete = true;
        r.summary = format!(
            "{}: {} validators, {} s, {} episodes ({} splits, {} partitions, {} kills), {} heals, resume max {} ms / p50 {} / p99 {}, {} safety violations, final height {}{}",
            if r.pass { "PASS" } else { "FAIL" },
            r.validators,
            r.duration_s,
            r.episodes,
            r.splits,
            r.partitions,
            r.kills,
            r.heals,
            r.max_resume_ms,
            r.p50_resume_ms,
            r.p99_resume_ms,
            r.safety_violations,
            r.final_height,
            if reasons.is_empty() { String::new() } else { format!(" [{}]", reasons.join("; ")) }
        );
        if let Err(e) = self.report.write(&self.report_path) {
            eprintln!("cannot write report {}: {e}", self.report_path.display());
        }
        self.say(&self.report.summary);
    }
}
