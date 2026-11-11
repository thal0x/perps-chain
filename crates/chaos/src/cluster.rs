//! `hlnode` process management: testnet generation, spawn / kill -9 /
//! restart, admin API access and background status pollers.

use crate::http;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::fs::OpenOptions;
use std::net::TcpListener;
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const CONSOLE_LOG: &str = "console.log";
/// Captured stdout/stderr of a node is rotated (`console.log.1`) at this size.
pub const CONSOLE_MAX_BYTES: u64 = 32 << 20;

/// `STATE DIVERGENCE` lines seen on a node's console (counted by the capture
/// thread so rotation can never lose one).
#[derive(Debug, Default)]
pub struct ConsoleDivergence {
    pub count: std::sync::atomic::AtomicU64,
    pub lines: Mutex<Vec<String>>,
}

/// Copy `src` line by line into `path`, rotating at `max_bytes`, and
/// record divergence lines.
pub fn capture_console<R: std::io::Read + Send + 'static>(src: R, path: PathBuf, max_bytes: u64, div: Arc<ConsoleDivergence>) {
    std::thread::spawn(move || {
        use std::io::{BufRead, Write};
        let mut r = std::io::BufReader::new(src);
        let open = |p: &Path| OpenOptions::new().create(true).append(true).open(p).ok();
        let mut f = open(&path);
        let mut size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut line = Vec::new();
        loop {
            line.clear();
            match r.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if line.len() > 64 * 1024 {
                line.truncate(64 * 1024);
                line.push(b'\n');
            }
            let text = String::from_utf8_lossy(&line);
            if text.contains("STATE DIVERGENCE") {
                div.count.fetch_add(1, Ordering::SeqCst);
                if let Ok(mut v) = div.lines.lock() {
                    if v.len() < 50 {
                        v.push(text.trim_end().chars().take(400).collect());
                    }
                }
            }
            if size + line.len() as u64 > max_bytes {
                drop(f.take());
                let _ = std::fs::rename(&path, path.with_extension("log.1"));
                f = open(&path);
                size = 0;
            }
            if let Some(w) = f.as_mut() {
                if w.write_all(&line).is_ok() {
                    size += line.len() as u64;
                }
            }
        }
    });
}

#[derive(Clone, Debug)]
pub struct NodeSpec {
    pub idx: usize,
    pub home: PathBuf,
    /// `127.0.0.1:port` of the HTTP API.
    pub api: String,
    pub p2p_listen: Option<String>,
    /// Validator address (`0x..` lowercase hex).
    pub address: String,
}

/// Latest `/admin/status` observation of one node.
#[derive(Clone, Debug, Default)]
pub struct NodeObs {
    pub up: bool,
    pub committed_height: u64,
    pub executed_height: u64,
    pub view: u64,
    pub peers: usize,
    pub last_ok: Option<Instant>,
}

pub fn parse_status(body: &[u8]) -> Option<NodeObs> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let num = |k: &str| v.get(k).and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok())));
    Some(NodeObs {
        up: true,
        committed_height: num("committed_height")?,
        executed_height: num("executed_height").unwrap_or(0),
        view: num("view").unwrap_or(0),
        peers: v.get("peers").and_then(|p| p.as_array()).map(|a| a.len()).unwrap_or(0),
        last_ok: Some(Instant::now()),
    })
}

/// True if `127.0.0.1:port` can be bound right now.
fn port_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Find `base` such that `base..base+n` (p2p) and `base+n+50..` (api) are all
/// free, starting from a seed-derived offset in 20000..55000.
pub fn find_free_ports(n: usize, seed: u64) -> Result<(u16, u16)> {
    let span = (2 * n + 60) as u64;
    for attempt in 0..200u64 {
        let base = 20_000 + ((seed.wrapping_mul(2654435761).wrapping_add(attempt * 7919)) % (35_000 - span)) as u16;
        let api = base + n as u16 + 50;
        if (0..n as u16).all(|i| port_free(base + i) && port_free(api + i)) {
            return Ok((base, api));
        }
    }
    bail!("could not find {n} free consecutive port pairs")
}

/// Extract `(address, p2p_addr)` pairs from genesis JSON
/// (`validators.validators[*]` or `validators[*]`).
pub fn genesis_validators(genesis: &Value) -> Vec<(String, Option<String>)> {
    let list = genesis
        .get("validators")
        .and_then(|v| v.get("validators").or(Some(v)))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    list.iter()
        .filter_map(|v| {
            let a = v.get("address")?.as_str()?.to_ascii_lowercase();
            let p = v.get("p2p_addr").and_then(|p| p.as_str()).map(|s| s.to_string());
            Some((a, p))
        })
        .collect()
}

pub fn address_of_key_file(path: &Path) -> Result<String> {
    let s = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let w = hlwire::Wallet::from_secret_hex(s.trim()).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    Ok(w.address().to_hex().to_ascii_lowercase())
}

pub struct Cluster {
    pub hlnode: PathBuf,
    pub net_dir: PathBuf,
    pub genesis: PathBuf,
    pub nodes: Vec<NodeSpec>,
    children: Vec<Option<Child>>,
    pub extra_args: Vec<String>,
    /// Shared "process is running" flags (load generator reads them).
    pub alive: Arc<Vec<AtomicBool>>,
    pub obs: Arc<Vec<Mutex<NodeObs>>>,
    pollers_stop: Arc<AtomicBool>,
    pub start_count: Vec<u64>,
    pub console_div: Vec<Arc<ConsoleDivergence>>,
}

impl Cluster {
    /// Run `hlnode testnet-gen` into `run_dir/net` and discover node specs.
    pub fn generate(
        hlnode: &Path,
        run_dir: &Path,
        n: usize,
        seed: u64,
        accounts: usize,
        extra_args: Vec<String>,
    ) -> Result<Self> {
        let net_dir = run_dir.join("net");
        let (p2p_base, api_base) = find_free_ports(n, seed)?;
        let out = Command::new(hlnode)
            .arg("testnet-gen")
            .args(["--validators", &n.to_string()])
            .arg("--out")
            .arg(&net_dir)
            .args(["--base-p2p", &p2p_base.to_string(), "--base-api", &api_base.to_string()])
            .args(["--accounts", &accounts.to_string(), "--seed", &seed.to_string()])
            .output()
            .with_context(|| format!("spawn {} testnet-gen", hlnode.display()))?;
        std::fs::write(run_dir.join("testnet-gen.log"), [&out.stdout[..], &out.stderr[..]].concat())?;
        if !out.status.success() {
            bail!("testnet-gen failed ({}): {}", out.status, String::from_utf8_lossy(&out.stderr));
        }
        let genesis = net_dir.join("genesis.json");
        let gv: Value = serde_json::from_slice(&std::fs::read(&genesis).context("read genesis.json")?).context("parse genesis.json")?;
        let vals = genesis_validators(&gv);
        let mut nodes = vec![];
        for i in 0..n {
            let home = net_dir.join(format!("node{i}"));
            if !home.is_dir() {
                bail!("testnet-gen did not create {}", home.display());
            }
            let addr = match address_of_key_file(&home.join("validator.key")) {
                Ok(a) => a,
                Err(e) => vals.get(i).map(|v| v.0.clone()).ok_or_else(|| anyhow!("no address for node{i}: {e}"))?,
            };
            let p2p = vals.iter().find(|v| v.0 == addr).and_then(|v| v.1.clone());
            nodes.push(NodeSpec { idx: i, home, api: format!("127.0.0.1:{}", api_base + i as u16), p2p_listen: p2p, address: addr });
        }
        Ok(Self {
            hlnode: hlnode.to_path_buf(),
            net_dir,
            genesis,
            children: (0..n).map(|_| None).collect(),
            extra_args,
            alive: Arc::new((0..n).map(|_| AtomicBool::new(false)).collect()),
            obs: Arc::new((0..n).map(|_| Mutex::new(NodeObs::default())).collect()),
            pollers_stop: Arc::new(AtomicBool::new(false)),
            start_count: vec![0; n],
            console_div: (0..n).map(|_| Arc::new(ConsoleDivergence::default())).collect(),
            nodes,
        })
    }

    pub fn n(&self) -> usize {
        self.nodes.len()
    }

    pub fn addresses(&self) -> Vec<String> {
        self.nodes.iter().map(|n| n.address.clone()).collect()
    }

    pub fn is_running(&self, i: usize) -> bool {
        self.children[i].is_some()
    }

    /// Spawn node i (must not be running). Must be called from the main
    /// thread (PDEATHSIG is tied to the spawning thread).
    pub fn start(&mut self, i: usize) -> Result<()> {
        if self.children[i].is_some() {
            bail!("node{i} already running");
        }
        let spec = &self.nodes[i];
        let mut cmd = Command::new(&self.hlnode);
        cmd.arg("run").arg("--home").arg(&spec.home).arg("--genesis").arg(&self.genesis);
        cmd.args(["--api-listen", &spec.api]);
        if let Some(p) = &spec.p2p_listen {
            cmd.args(["--p2p-listen", p]);
        }
        cmd.args(&self.extra_args);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // Die with the harness even if it is SIGKILLed. Linux-only; elsewhere
        // nodes are still stopped on normal exit and signals (D33).
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let mut child = cmd.spawn().with_context(|| format!("spawn node{i}"))?;
        let path = spec.home.join(CONSOLE_LOG);
        if let Some(o) = child.stdout.take() {
            capture_console(o, path.clone(), CONSOLE_MAX_BYTES, self.console_div[i].clone());
        }
        if let Some(e) = child.stderr.take() {
            capture_console(e, spec.home.join("console.err.log"), CONSOLE_MAX_BYTES, self.console_div[i].clone());
        }
        self.children[i] = Some(child);
        self.alive[i].store(true, Ordering::SeqCst);
        self.start_count[i] += 1;
        Ok(())
    }

    /// kill -9 and reap.
    pub fn kill(&mut self, i: usize) {
        if let Some(mut c) = self.children[i].take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.alive[i].store(false, Ordering::SeqCst);
        if let Ok(mut o) = self.obs[i].lock() {
            o.up = false;
        }
    }

    /// Reap children that exited on their own.
    pub fn poll_exits(&mut self) -> Vec<(usize, ExitStatus)> {
        let mut out = vec![];
        for i in 0..self.children.len() {
            if let Some(c) = self.children[i].as_mut() {
                if let Ok(Some(st)) = c.try_wait() {
                    out.push((i, st));
                    self.children[i] = None;
                    self.alive[i].store(false, Ordering::SeqCst);
                }
            }
        }
        out
    }

    /// SIGTERM everyone, wait up to `grace`, then SIGKILL.
    pub fn stop_all(&mut self, grace: Duration) {
        self.pollers_stop.store(true, Ordering::SeqCst);
        for c in self.children.iter().flatten() {
            unsafe {
                libc::kill(c.id() as i32, libc::SIGTERM);
            }
        }
        let deadline = Instant::now() + grace;
        for i in 0..self.children.len() {
            if let Some(mut c) = self.children[i].take() {
                loop {
                    match c.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                        _ => {
                            let _ = c.kill();
                            let _ = c.wait();
                            break;
                        }
                    }
                }
            }
            self.alive[i].store(false, Ordering::SeqCst);
        }
    }

    pub fn post_chaos(&self, i: usize, json: &str) -> Result<()> {
        let r = http::post_json(&self.nodes[i].api, "/admin/chaos", json, Duration::from_secs(2))?;
        if !r.ok() {
            bail!("node{i} /admin/chaos -> {}: {}", r.status, r.text());
        }
        Ok(())
    }

    pub fn status(&self, i: usize) -> Option<NodeObs> {
        let r = http::get(&self.nodes[i].api, "/admin/status", Duration::from_millis(800)).ok()?;
        if !r.ok() {
            return None;
        }
        parse_status(&r.body)
    }

    pub fn obs(&self, i: usize) -> NodeObs {
        self.obs[i].lock().map(|o| o.clone()).unwrap_or_default()
    }

    /// One polling thread per node: `/admin/status` every `every`.
    pub fn start_pollers(&self, every: Duration) {
        for i in 0..self.n() {
            let api = self.nodes[i].api.clone();
            let obs = self.obs.clone();
            let alive = self.alive.clone();
            let stop = self.pollers_stop.clone();
            std::thread::Builder::new()
                .name(format!("poll{i}"))
                .spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        let t0 = Instant::now();
                        if alive[i].load(Ordering::SeqCst) {
                            let got = http::get(&api, "/admin/status", Duration::from_millis(800))
                                .ok()
                                .filter(|r| r.ok())
                                .and_then(|r| parse_status(&r.body));
                            if let Ok(mut o) = obs[i].lock() {
                                match got {
                                    Some(n) => *o = n,
                                    None => o.up = false,
                                }
                            }
                        }
                        let el = t0.elapsed();
                        if el < every {
                            std::thread::sleep(every - el);
                        }
                    }
                })
                .expect("spawn poller");
        }
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.pollers_stop.store(true, Ordering::SeqCst);
        for c in self.children.iter_mut().flatten() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parsing() {
        let s = br#"{"validator":"0x01","committed_height":12,"executed_height":10,"view":15,"exec_root":"0x00","mempool_len":0,"peers":["0x02","0x03"],"last_commit_ms":5}"#;
        let o = parse_status(s).unwrap();
        assert_eq!((o.committed_height, o.executed_height, o.view, o.peers), (12, 10, 15, 2));
        assert!(parse_status(b"{}").is_none());
        assert!(parse_status(b"nope").is_none());
    }

    #[test]
    fn genesis_validator_extraction() {
        let g: Value = serde_json::from_str(r#"{"validators":{"validators":[{"address":"0xAB","stake":1,"p2p_addr":"127.0.0.1:1"}]}}"#).unwrap();
        assert_eq!(genesis_validators(&g), vec![("0xab".to_string(), Some("127.0.0.1:1".to_string()))]);
        let g: Value = serde_json::from_str(r#"{"validators":[{"address":"0xcd"}]}"#).unwrap();
        assert_eq!(genesis_validators(&g), vec![("0xcd".to_string(), None)]);
    }

    #[test]
    fn console_capture_rotates_and_counts() {
        let d = crate::testutil::tempdir("console");
        let p = d.join("console.log");
        let div = Arc::new(ConsoleDivergence::default());
        let mut data = String::new();
        for i in 0..200 {
            data.push_str(&format!("line {i} {}\n", "x".repeat(50)));
        }
        data.push_str("ERROR STATE DIVERGENCE at 5\n");
        capture_console(std::io::Cursor::new(data.into_bytes()), p.clone(), 4096, div.clone());
        let t0 = Instant::now();
        while div.count.load(Ordering::SeqCst) == 0 && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(div.count.load(Ordering::SeqCst), 1);
        assert!(std::fs::metadata(&p).unwrap().len() <= 4096);
        assert!(std::fs::metadata(d.join("console.log.1")).unwrap().len() <= 4096);
    }

    #[test]
    fn free_ports_found() {
        let (p, a) = find_free_ports(7, 1234).unwrap();
        assert!(a > p + 7);
        assert!(port_free(p) && port_free(a + 6));
    }
}
