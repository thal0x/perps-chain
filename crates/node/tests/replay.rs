//! `hlnode run --replay-log` (D8, AC2 in miniature): a small `sim gen` log
//! replayed by 4 real `hlnode` processes over real consensus, twice.
//! All 4 roots.log equal each other, equal `sim replay` roots, and the two
//! runs are identical. A mismatching genesis is refused.

use interfaces::*;
use sim::gen::{generate, keys_for, GenConfig};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_hlnode");

fn free_base(span: u16) -> u16 {
    use std::net::TcpListener;
    let seed = (std::process::id() as u64 * 104_729 + 17) % 20_000;
    let mut base = 30_000 + (seed as u16 / 16) * 16;
    loop {
        if (0..span).all(|i| TcpListener::bind(("127.0.0.1", base + i)).is_ok()) {
            return base;
        }
        base = if base > 60_000 { 30_000 } else { base + span + 5 };
    }
}

struct Procs(Vec<Child>);
impl Drop for Procs {
    fn drop(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn roots_of(p: &Path) -> BTreeMap<Height, String> {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.to_string()))
        })
        .collect()
}

struct Fixture {
    dir: PathBuf,
    log: PathBuf,
    genesis_json: PathBuf,
    keys: sim::SimKeys,
    base_api: u16,
    blocks: u64,
}

fn fixture(name: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("hlnode-replay-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let base = free_base(10);
    let mut cfg = GenConfig::small(42);
    cfg.actions = 4_000;
    cfg.sim_ms = 60_000;
    cfg.block_ms = 1_000;
    cfg.accounts = 80;
    cfg.genesis_opts.p2p_base_port = base;
    let (g, keys) = keys_for(&cfg);
    sim::genesis::write_genesis_files(&dir, &g, &keys).unwrap();
    let log = dir.join("small.log.zst");
    let s = generate(&cfg, &sim::state_factory(), std::fs::File::create(&log).unwrap(), |_, _| {}).unwrap();
    assert_eq!(s.footer.blocks, 60);
    assert!(s.footer.actions >= 3_000, "{:?}", s.footer);
    Fixture { genesis_json: dir.join("genesis.json"), dir, log, keys, base_api: base + 5, blocks: 60 }
}

impl Fixture {
    fn home(&self, run: &str, i: usize) -> PathBuf {
        self.dir.join(run).join(format!("node{i}"))
    }

    fn spawn(&self, run: &str, i: usize, genesis: Option<&Path>) -> Child {
        let home = self.home(run, i);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("validator.key"), self.keys.validators[i].secret_hex()).unwrap();
        let p2p = self.keys_p2p(i);
        let out = std::fs::File::create(home.join("stdout.log")).unwrap();
        let mut c = Command::new(BIN);
        c.args(["run", "--home"]).arg(&home).arg("--replay-log").arg(&self.log);
        c.args(["--p2p-listen", &p2p, "--api-listen", &format!("127.0.0.1:{}", self.base_api + i as u16)]);
        c.args(["--min-block-ms", "30"]);
        // without --genesis the default path (<home>/../genesis.json) is absent: genesis from the log header
        if let Some(g) = genesis {
            c.arg("--genesis").arg(g);
        }
        c.stdout(out.try_clone().unwrap()).stderr(out).spawn().unwrap()
    }

    fn keys_p2p(&self, i: usize) -> String {
        let g: Genesis = serde_json::from_slice(&std::fs::read(&self.genesis_json).unwrap()).unwrap();
        g.validators.validators[i].p2p_addr.clone()
    }

    /// Run 4 validators until all wrote replay.json; returns node0's roots.
    fn run(&self, run: &str) -> BTreeMap<Height, String> {
        let procs = Procs(
            (0..4).map(|i| self.spawn(run, i, if i % 2 == 0 { Some(self.genesis_json.as_path()) } else { None })).collect(),
        );
        let t = Instant::now();
        loop {
            if (0..4).all(|i| self.home(run, i).join("replay.json").exists()) {
                break;
            }
            if t.elapsed() > Duration::from_secs(240) {
                for i in 0..4 {
                    let s = std::fs::read_to_string(self.home(run, i).join("node.log")).unwrap_or_default();
                    eprintln!("---- node{i} ----\n{}", s.lines().rev().take(20).collect::<Vec<_>>().join("\n"));
                }
                panic!("replay {run} did not finish");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        drop(procs);
        let r0 = roots_of(&self.home(run, 0).join("roots.log"));
        assert_eq!(r0.len() as u64, self.blocks, "roots.log covers exactly the logged heights");
        assert_eq!(r0.keys().next_back().copied(), Some(self.blocks));
        for i in 0..4 {
            let home = self.home(run, i);
            assert_eq!(roots_of(&home.join("roots.log")), r0, "{run}: node{i} roots differ from node0");
            let j: serde_json::Value = serde_json::from_slice(&std::fs::read(home.join("replay.json")).unwrap()).unwrap();
            assert_eq!(j["last_height"], self.blocks);
            assert_eq!(j["invariant_checks"], self.blocks, "invariants every block");
            assert_eq!(j["violations"], 0, "{j}");
            assert_eq!(j["divergences"], 0);
            let nl = std::fs::read_to_string(home.join("node.log")).unwrap();
            assert!(!nl.contains("STATE DIVERGENCE"), "node{i}: divergence");
            assert!(!nl.contains("differs from replay override"), "node{i}: override mismatch");
        }
        r0
    }
}

#[test]
fn replay_log_four_validators_twice_matches_offline() {
    let f = fixture("ac2mini");
    // offline reference
    let mut out = Vec::new();
    let s = sim::replay::replay(
        std::fs::File::open(&f.log).unwrap(),
        &sim::state_factory(),
        &sim::replay::ReplayOpts::default(),
        Some(&mut out),
    )
    .unwrap();
    assert!(s.violations.is_empty(), "{:?}", s.violations);
    let offline: BTreeMap<Height, String> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| {
            let (h, r) = l.split_once(' ').unwrap();
            (h.parse().unwrap(), r.trim_start_matches("0x").to_string())
        })
        .collect();
    let a = f.run("a");
    let b = f.run("b");
    assert_eq!(a, offline, "network roots == sim replay roots");
    assert_eq!(a, b, "run A == run B");
    let j: serde_json::Value = serde_json::from_slice(&std::fs::read(f.home("a", 0).join("replay.json")).unwrap()).unwrap();
    assert_eq!(j["roots_digest"].as_str().unwrap(), s.roots_digest.to_hex(), "roots digest == sim replay's");
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn replay_log_refuses_mismatching_genesis() {
    let f = fixture("badgen");
    let mut g: Genesis = serde_json::from_slice(&std::fs::read(&f.genesis_json).unwrap()).unwrap();
    g.balances[0].1 = Usd::from_raw(g.balances[0].1.raw() + 1);
    let bad = f.dir.join("bad_genesis.json");
    std::fs::write(&bad, serde_json::to_vec(&g).unwrap()).unwrap();
    let mut c = f.spawn("bad", 0, Some(&bad));
    let t = Instant::now();
    let st = loop {
        if let Some(s) = c.try_wait().unwrap() {
            break s;
        }
        assert!(t.elapsed() < Duration::from_secs(30), "node did not refuse the genesis");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!st.success());
    let out = std::fs::read_to_string(f.home("bad", 0).join("stdout.log")).unwrap();
    assert!(out.contains("genesis mismatch"), "{out}");
    let _ = std::fs::remove_dir_all(&f.dir);
}
