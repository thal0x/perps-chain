//! Process-level tests: real `hlnode` processes on random localhost ports.

use interfaces::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_hlnode");

struct Net {
    dir: PathBuf,
    n: usize,
    p2p_ports: Vec<u16>,
    api_ports: Vec<u16>,
    procs: Vec<Option<Child>>,
    http: reqwest::blocking::Client,
}

/// Reserve `k` distinct free ports by binding 127.0.0.1:0; the listeners are
/// held (so nobody else gets these ports) until the caller drops them right
/// before spawning the nodes.
fn reserve_ports(k: usize) -> (Vec<std::net::TcpListener>, Vec<u16>) {
    let ls: Vec<_> = (0..k)
        .map(|_| std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind 127.0.0.1:0"))
        .collect();
    let ports = ls.iter().map(|l| l.local_addr().unwrap().port()).collect();
    (ls, ports)
}

fn addr_in_use(log: &str) -> bool {
    let l = log.to_lowercase();
    l.contains("addrinuse") || l.contains("address already in use") || l.contains("address in use")
}

impl Net {
    /// Start an n-validator testnet; if a node fails to bind its ports
    /// (AddrInUse, e.g. a port taken between reservation and spawn) the whole
    /// cluster is retried with fresh ports (max 3 attempts). Any other early
    /// exit fails immediately with the node's log.
    fn new(name: &str, n: usize, extra_gen: &[&str]) -> Net {
        let mut last = String::new();
        for attempt in 1..=3 {
            match Net::try_new(name, n, extra_gen) {
                Ok(net) => return net,
                Err(e) => {
                    eprintln!("testnet {name}: attempt {attempt}: port conflict ({e}); retrying with fresh ports");
                    last = e;
                }
            }
        }
        panic!("testnet {name}: port conflicts on 3 attempts: {last}")
    }

    fn try_new(name: &str, n: usize, extra_gen: &[&str]) -> Result<Net, String> {
        let dir = std::env::temp_dir().join(format!("hlnode-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (listeners, ports) = reserve_ports(2 * n);
        let (p2p_ports, api_ports) = (ports[..n].to_vec(), ports[n..].to_vec());
        let mut args = vec![
            "testnet-gen".to_string(),
            "--validators".into(),
            n.to_string(),
            "--out".into(),
            dir.display().to_string(),
            "--fund-sdk-examples".into(),
            "--accounts".into(),
            "2".into(),
        ];
        args.extend(extra_gen.iter().map(|s| s.to_string()));
        let st = Command::new(BIN)
            .args(&args)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success());
        // point the genesis p2p addresses at the reserved ports (listen flags are passed at run)
        let gp = dir.join("genesis.json");
        let mut g: Value = serde_json::from_slice(&std::fs::read(&gp).unwrap()).unwrap();
        for (i, v) in g["validators"]["validators"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            v["p2p_addr"] = json!(format!("127.0.0.1:{}", p2p_ports[i]));
        }
        std::fs::write(&gp, serde_json::to_vec_pretty(&g).unwrap()).unwrap();
        let mut net = Net {
            dir,
            n,
            p2p_ports,
            api_ports,
            procs: (0..n).map(|_| None).collect(),
            http: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap(),
        };
        drop(listeners);
        for i in 0..n {
            net.spawn(i);
        }
        // up = every API answers; fail fast if a process exits
        let t = Instant::now();
        loop {
            let mut up = 0;
            for i in 0..n {
                if let Some(code) = net.exited(i) {
                    let log = net.node_output(i);
                    if addr_in_use(&log) {
                        return Err(format!("node{i} exited ({code}): address in use"));
                    }
                    net.dump_logs();
                    panic!("node{i} exited during startup ({code}):\n{log}");
                }
                if net.status(i).is_some() {
                    up += 1;
                }
            }
            if up == n {
                return Ok(net);
            }
            if t.elapsed() > Duration::from_secs(60) {
                net.dump_logs();
                panic!("testnet {name}: nodes not up within 60 s");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn home(&self, i: usize) -> PathBuf {
        self.dir.join(format!("node{i}"))
    }

    fn spawn(&mut self, i: usize) {
        let out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.home(i).join("stdout.log"))
            .unwrap();
        let c = Command::new(BIN)
            .args(["run", "--home"])
            .arg(self.home(i))
            .args(["--p2p-listen", &format!("127.0.0.1:{}", self.p2p_ports[i])])
            .args(["--api-listen", &format!("127.0.0.1:{}", self.api_ports[i])])
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .spawn()
            .unwrap();
        self.procs[i] = Some(c);
    }

    /// (Re)start node i on its ports. A restart can race another process for
    /// the port the node just released: on AddrInUse, retry for a few seconds.
    fn start(&mut self, i: usize) {
        for attempt in 0..10 {
            let mark = self.node_output(i).len();
            self.spawn(i);
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(3) {
                if self.status(i).is_some() {
                    return;
                }
                if let Some(code) = self.exited(i) {
                    let log = self.node_output(i);
                    if addr_in_use(log.get(mark..).unwrap_or("")) && attempt < 9 {
                        break;
                    }
                    panic!("node{i} exited on restart ({code}):\n{log}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if self.procs[i].is_some() {
                return; // alive, still starting up: wait_height watches it
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Exit status if node i's process has exited (the child is reaped).
    fn exited(&mut self, i: usize) -> Option<std::process::ExitStatus> {
        let st = self.procs[i].as_mut()?.try_wait().ok()??;
        self.procs[i] = None;
        Some(st)
    }

    fn node_output(&self, i: usize) -> String {
        let a = std::fs::read_to_string(self.home(i).join("stdout.log")).unwrap_or_default();
        let b = std::fs::read_to_string(self.home(i).join("node.log")).unwrap_or_default();
        format!("{a}{b}")
    }

    fn kill9(&mut self, i: usize) {
        if let Some(mut c) = self.procs[i].take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    fn sigterm(&mut self, i: usize) -> std::process::ExitStatus {
        let mut c = self.procs[i].take().unwrap();
        unsafe_kill(c.id(), 15);
        let t = Instant::now();
        loop {
            if let Some(s) = c.try_wait().unwrap() {
                return s;
            }
            assert!(
                t.elapsed() < Duration::from_secs(20),
                "node did not exit on SIGTERM"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn url(&self, i: usize, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.api_ports[i], path)
    }

    fn status(&self, i: usize) -> Option<NodeStatus> {
        self.http
            .get(self.url(i, "/admin/status"))
            .send()
            .ok()?
            .json()
            .ok()
    }

    fn post(&self, i: usize, path: &str, body: &Value) -> Value {
        self.http
            .post(self.url(i, path))
            .json(body)
            .send()
            .unwrap()
            .json()
            .unwrap()
    }

    fn wait_height(&mut self, nodes: &[usize], h: Height, timeout: Duration) {
        let t = Instant::now();
        loop {
            for &i in nodes {
                if let Some(code) = self.exited(i) {
                    self.dump_logs();
                    panic!(
                        "node{i} exited ({code}) while waiting for height {h}:\n{}",
                        self.node_output(i)
                    );
                }
            }
            let hs: Vec<Option<Height>> = nodes
                .iter()
                .map(|&i| {
                    self.status(i)
                        .map(|s| s.committed_height.min(s.executed_height))
                })
                .collect();
            if hs.iter().all(|x| x.is_some_and(|x| x >= h)) {
                return;
            }
            if t.elapsed() > timeout {
                self.dump_logs();
                panic!("timeout waiting for height {h}: {hs:?}");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn max_height(&self) -> Height {
        (0..self.n)
            .filter_map(|i| self.status(i))
            .map(|s| s.executed_height)
            .max()
            .unwrap_or(0)
    }

    fn dump_logs(&self) {
        for i in 0..self.n {
            let p = self.home(i).join("node.log");
            let s = std::fs::read_to_string(&p).unwrap_or_default();
            let tail: Vec<&str> = s.lines().rev().take(15).collect();
            eprintln!("---- node{i} ----");
            for l in tail.iter().rev() {
                eprintln!("{l}");
            }
        }
    }

    fn sdk_wallet(&self) -> hlwire::Wallet {
        let a: Value =
            serde_json::from_slice(&std::fs::read(self.dir.join("accounts.json")).unwrap())
                .unwrap();
        let s = a
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == "sdk_example")
            .unwrap()["secret"]
            .as_str()
            .unwrap()
            .to_string();
        hlwire::Wallet::from_secret_hex(&s).unwrap()
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        for i in 0..self.n {
            self.kill9(i);
        }
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        } else {
            eprintln!("test dir kept: {}", self.dir.display());
        }
    }
}

fn unsafe_kill(pid: u32, sig: i32) {
    let _ = Command::new("kill")
        .arg(format!("-{sig}"))
        .arg(pid.to_string())
        .status();
}

fn read_log(p: &Path) -> BTreeMap<Height, String> {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.to_string()))
        })
        .collect()
}

/// Every height present in two or more nodes' logs must agree. Returns the
/// number of heights compared across all nodes.
fn assert_logs_agree(net: &Net, file: &str) -> usize {
    let logs: Vec<_> = (0..net.n)
        .map(|i| read_log(&net.home(i).join(file)))
        .collect();
    let mut all: BTreeMap<Height, String> = BTreeMap::new();
    for (i, l) in logs.iter().enumerate() {
        for (h, v) in l {
            if let Some(prev) = all.get(h) {
                assert_eq!(prev, v, "{file} mismatch at height {h} (node{i})");
            } else {
                all.insert(*h, v.clone());
            }
        }
    }
    let common = logs
        .iter()
        .map(|l| l.keys().next_back().copied().unwrap_or(0))
        .min()
        .unwrap_or(0);
    common as usize
}

fn place_order(net: &Net, node: usize, w: &hlwire::Wallet, px: &str, nonce: u64) -> Value {
    let order = OrderRequest {
        asset: 0,
        is_buy: true,
        limit_px: Px::parse(px).unwrap(),
        sz: Sz::parse("0.001").unwrap(),
        reduce_only: false,
        kind: OrderKind::Limit { tif: Tif::Gtc },
        cloid: None,
    };
    let action = hlwire::order_action(&[order], Grouping::Na);
    let body = hlwire::signed_l1_request(w, action, nonce, None, None, false);
    net.post(node, "/exchange", &body)
}

fn resting_oid(resp: &Value) -> u64 {
    assert_eq!(resp["status"], "ok", "order response: {resp}");
    let st = &resp["response"]["data"]["statuses"][0];
    st["resting"]["oid"]
        .as_u64()
        .unwrap_or_else(|| panic!("not resting: {resp}"))
}

fn open_oids(net: &Net, node: usize, user: &Address) -> Vec<u64> {
    let v = net.post(
        node,
        "/info",
        &json!({"type": "openOrders", "user": user.to_hex()}),
    );
    v.as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|o| o["oid"].as_u64())
        .collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn four_validators_orders_roots_and_crash_recovery() {
    let mut net = Net::new("four", 4, &[]);
    net.wait_height(&[0, 1, 2, 3], 3, Duration::from_secs(60));

    // an order through node 1 rests and is visible on all nodes
    let w = net.sdk_wallet();
    let resp = place_order(&net, 1, &w, "50000", now_ms());
    let oid = resting_oid(&resp);
    let h = net.max_height();
    net.wait_height(&[0, 1, 2, 3], h, Duration::from_secs(30));
    for i in 0..4 {
        assert!(
            open_oids(&net, i, &w.address()).contains(&oid),
            "node{i} does not show oid {oid}"
        );
    }

    // kill -9 node 3 while the others continue
    net.kill9(3);
    let resp = place_order(&net, 0, &w, "49000", now_ms());
    let oid2 = resting_oid(&resp);
    let h = net.max_height() + 8;
    net.wait_height(&[0, 1, 2], h, Duration::from_secs(60));

    // restart: it resumes from its FileStore and catches up
    net.start(3);
    let h = net.max_height() + 3;
    net.wait_height(&[0, 1, 2, 3], h, Duration::from_secs(60));
    let oids = open_oids(&net, 3, &w.address());
    assert!(
        oids.contains(&oid) && oids.contains(&oid2),
        "restarted node3 open orders {oids:?}"
    );

    let common = assert_logs_agree(&net, "roots.log");
    assert!(common as u64 >= h, "roots compared up to {common}");
    assert_logs_agree(&net, "commits.log");
    for i in 0..4 {
        let log = std::fs::read_to_string(net.home(i).join("node.log")).unwrap();
        assert!(
            !log.contains("STATE DIVERGENCE"),
            "node{i} logged a divergence"
        );
    }
    // roots.log has one line per height (no duplicates after restart)
    let s = std::fs::read_to_string(net.home(3).join("roots.log")).unwrap();
    let hs: Vec<u64> = s
        .lines()
        .map(|l| l.split(' ').next().unwrap().parse().unwrap())
        .collect();
    assert!(
        hs.windows(2).all(|w| w[1] == w[0] + 1),
        "roots.log heights not contiguous"
    );
}

#[test]
fn single_validator_m2() {
    let mut net = Net::new("single", 1, &[]);
    // exercise the snapshot path: snapshot every 5 heights (restart restores it)
    net.kill9(0);
    let cfgp = net.home(0).join("config.toml");
    let c = std::fs::read_to_string(&cfgp).unwrap();
    std::fs::write(&cfgp, format!("snapshot_interval = 5\n{c}")).unwrap();
    let _ = std::fs::remove_dir_all(net.home(0).join("data"));
    for f in ["roots.log", "commits.log", "node.log"] {
        let _ = std::fs::remove_file(net.home(0).join(f));
    }
    net.start(0);
    net.wait_height(&[0], 3, Duration::from_secs(60));
    let st = net.status(0).unwrap();
    assert_eq!(st.validator.unwrap(), {
        let k = std::fs::read_to_string(net.home(0).join("validator.key")).unwrap();
        hlwire::Wallet::from_secret_hex(k.trim()).unwrap().address()
    });
    let w = net.sdk_wallet();
    let oid = resting_oid(&place_order(&net, 0, &w, "50000", now_ms()));
    assert!(open_oids(&net, 0, &w.address()).contains(&oid));
    // clearinghouseState shows the genesis funding
    let ch = net.post(
        0,
        "/info",
        &json!({"type": "clearinghouseState", "user": w.address().to_hex()}),
    );
    assert!(
        ch["marginSummary"]["accountValue"].as_str().is_some(),
        "{ch}"
    );

    // graceful SIGTERM => exit 0; restart resumes at >= the old height
    let before = net.status(0).unwrap().executed_height;
    let code = net.sigterm(0);
    assert!(code.success(), "exit status {code:?}");
    net.start(0);
    net.wait_height(&[0], before + 2, Duration::from_secs(60));
    assert!(
        open_oids(&net, 0, &w.address()).contains(&oid),
        "order lost across restart"
    );
    let log = std::fs::read_to_string(net.home(0).join("node.log")).unwrap();
    assert!(!log.contains("STATE DIVERGENCE"));
    assert!(
        log.contains("restored from snapshot"),
        "restart did not use a snapshot"
    );

    // kill -9 and restart again (snapshot + re-executed blocks verified against roots.log)
    let before = net.status(0).unwrap().executed_height;
    net.kill9(0);
    net.start(0);
    net.wait_height(&[0], before + 2, Duration::from_secs(60));
    let log = std::fs::read_to_string(net.home(0).join("node.log")).unwrap();
    assert!(!log.contains("STATE DIVERGENCE"));
}

/// D29: a validator down long enough that every peer pruned the blocks it
/// needs (retention 20 heights, 4 KiB segments, snapshots every 5) recovers by snapshot sync
/// (certified snapshot root), then follows the chain with matching roots.
#[test]
fn far_lagging_node_recovers_via_snapshot_sync() {
    let mut net = Net::new("snapsync", 4, &[]);
    // small retention + snapshot period on every node, from genesis on
    for i in 0..4 {
        net.kill9(i);
        let cfgp = net.home(i).join("config.toml");
        let c = std::fs::read_to_string(&cfgp).unwrap();
        std::fs::write(&cfgp, format!("snapshot_interval = 5\nretain_heights = 20\nsegment_bytes = 4096\n{c}")).unwrap();
        let _ = std::fs::remove_dir_all(net.home(i).join("data"));
        for f in ["roots.log", "commits.log", "node.log"] {
            let _ = std::fs::remove_file(net.home(i).join(f));
        }
    }
    for i in 0..4 {
        net.start(i);
    }
    net.wait_height(&[0, 1, 2, 3], 6, Duration::from_secs(60));
    net.kill9(3);
    let down_at = net.max_height();
    // far beyond retention: peers keep only ~20 heights (whole segments are pruned,
    // so go well past it)
    net.wait_height(&[0, 1, 2], down_at + 80, Duration::from_secs(120));
    net.start(3);
    let h = net.max_height() + 5;
    net.wait_height(&[0, 1, 2, 3], h, Duration::from_secs(120));
    let log3 = std::fs::read_to_string(net.home(3).join("node.log")).unwrap();
    assert!(log3.contains("installed state snapshot"), "node3 did not install a snapshot:\n{}", &log3[log3.len().saturating_sub(3000)..]);
    // roots agree on every height logged by more than one node, and node3 has roots above its snapshot
    let common = assert_logs_agree(&net, "roots.log");
    assert!(common as u64 >= h, "roots compared up to {common}");
    assert_logs_agree(&net, "commits.log");
    for i in 0..4 {
        let log = std::fs::read_to_string(net.home(i).join("node.log")).unwrap();
        assert!(!log.contains("STATE DIVERGENCE"), "node{i} logged a divergence");
    }
    let r3 = read_log(&net.home(3).join("roots.log"));
    let r0 = read_log(&net.home(0).join("roots.log"));
    let after: Vec<_> = r3.keys().filter(|k| **k > down_at + 20).copied().collect();
    assert!(!after.is_empty(), "node3 executed nothing after recovering");
    for k in after {
        assert_eq!(r3.get(&k), r0.get(&k), "root at {k}");
    }
}
