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
    base_api: u16,
    procs: Vec<Option<Child>>,
    http: reqwest::blocking::Client,
}

fn free_base(span: u16) -> u16 {
    use std::net::TcpListener;
    let seed = (std::process::id() as u64 * 7919 + Instant::now().elapsed().as_nanos() as u64) % 20_000;
    let mut base = 20_000 + (seed as u16 / 16) * 16;
    loop {
        if (0..span).all(|i| TcpListener::bind(("127.0.0.1", base + i)).is_ok()) {
            return base;
        }
        base = if base > 60_000 { 20_000 } else { base + span + 3 };
    }
}

impl Net {
    fn new(name: &str, n: usize, extra_gen: &[&str]) -> Net {
        let dir = std::env::temp_dir().join(format!("hlnode-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let base = free_base(2 * n as u16 + 2);
        let (base_p2p, base_api) = (base, base + n as u16 + 1);
        let mut args = vec![
            "testnet-gen".to_string(),
            "--validators".into(),
            n.to_string(),
            "--out".into(),
            dir.display().to_string(),
            "--base-p2p".into(),
            base_p2p.to_string(),
            "--base-api".into(),
            base_api.to_string(),
            "--fund-sdk-examples".into(),
            "--accounts".into(),
            "2".into(),
        ];
        args.extend(extra_gen.iter().map(|s| s.to_string()));
        let st = Command::new(BIN).args(&args).stdout(Stdio::null()).status().unwrap();
        assert!(st.success());
        let mut net = Net {
            dir,
            n,
            base_api,
            procs: (0..n).map(|_| None).collect(),
            http: reqwest::blocking::Client::builder().timeout(Duration::from_secs(15)).build().unwrap(),
        };
        for i in 0..n {
            net.start(i);
        }
        net
    }

    fn home(&self, i: usize) -> PathBuf {
        self.dir.join(format!("node{i}"))
    }

    fn start(&mut self, i: usize) {
        let out = std::fs::OpenOptions::new().create(true).append(true).open(self.home(i).join("stdout.log")).unwrap();
        let c = Command::new(BIN)
            .args(["run", "--home"])
            .arg(self.home(i))
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .spawn()
            .unwrap();
        self.procs[i] = Some(c);
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
            assert!(t.elapsed() < Duration::from_secs(20), "node did not exit on SIGTERM");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn url(&self, i: usize, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.base_api + i as u16, path)
    }

    fn status(&self, i: usize) -> Option<NodeStatus> {
        self.http.get(self.url(i, "/admin/status")).send().ok()?.json().ok()
    }

    fn post(&self, i: usize, path: &str, body: &Value) -> Value {
        self.http.post(self.url(i, path)).json(body).send().unwrap().json().unwrap()
    }

    fn wait_height(&self, nodes: &[usize], h: Height, timeout: Duration) {
        let t = Instant::now();
        loop {
            let hs: Vec<Option<Height>> =
                nodes.iter().map(|&i| self.status(i).map(|s| s.committed_height.min(s.executed_height))).collect();
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
        (0..self.n).filter_map(|i| self.status(i)).map(|s| s.executed_height).max().unwrap_or(0)
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
        let a: Value = serde_json::from_slice(&std::fs::read(self.dir.join("accounts.json")).unwrap()).unwrap();
        let s = a.as_array().unwrap().iter().find(|x| x["name"] == "sdk_example").unwrap()["secret"].as_str().unwrap().to_string();
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
    let _ = Command::new("kill").arg(format!("-{sig}")).arg(pid.to_string()).status();
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
    let logs: Vec<_> = (0..net.n).map(|i| read_log(&net.home(i).join(file))).collect();
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
    let common = logs.iter().map(|l| l.keys().next_back().copied().unwrap_or(0)).min().unwrap_or(0);
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
    st["resting"]["oid"].as_u64().unwrap_or_else(|| panic!("not resting: {resp}"))
}

fn open_oids(net: &Net, node: usize, user: &Address) -> Vec<u64> {
    let v = net.post(node, "/info", &json!({"type": "openOrders", "user": user.to_hex()}));
    v.as_array().unwrap_or(&vec![]).iter().filter_map(|o| o["oid"].as_u64()).collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
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
        assert!(open_oids(&net, i, &w.address()).contains(&oid), "node{i} does not show oid {oid}");
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
    assert!(oids.contains(&oid) && oids.contains(&oid2), "restarted node3 open orders {oids:?}");

    let common = assert_logs_agree(&net, "roots.log");
    assert!(common as u64 >= h, "roots compared up to {common}");
    assert_logs_agree(&net, "commits.log");
    for i in 0..4 {
        let log = std::fs::read_to_string(net.home(i).join("node.log")).unwrap();
        assert!(!log.contains("STATE DIVERGENCE"), "node{i} logged a divergence");
    }
    // roots.log has one line per height (no duplicates after restart)
    let s = std::fs::read_to_string(net.home(3).join("roots.log")).unwrap();
    let hs: Vec<u64> = s.lines().map(|l| l.split(' ').next().unwrap().parse().unwrap()).collect();
    assert!(hs.windows(2).all(|w| w[1] == w[0] + 1), "roots.log heights not contiguous");
}

#[test]
fn single_validator_m2() {
    let mut net = Net::new("single", 1, &[]);
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
    let ch = net.post(0, "/info", &json!({"type": "clearinghouseState", "user": w.address().to_hex()}));
    assert!(ch["marginSummary"]["accountValue"].as_str().is_some(), "{ch}");

    // graceful SIGTERM => exit 0; restart resumes at >= the old height
    let before = net.status(0).unwrap().executed_height;
    let code = net.sigterm(0);
    assert!(code.success(), "exit status {code:?}");
    net.start(0);
    net.wait_height(&[0], before + 2, Duration::from_secs(60));
    assert!(open_oids(&net, 0, &w.address()).contains(&oid), "order lost across restart");
    let log = std::fs::read_to_string(net.home(0).join("node.log")).unwrap();
    assert!(!log.contains("STATE DIVERGENCE"));
}
