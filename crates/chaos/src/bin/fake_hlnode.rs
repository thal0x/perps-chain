//! A tiny stand-in for `hlnode` implementing just the NODE_CLI.md process
//! contract the harness relies on (`testnet-gen`, `run`, `commits.log`,
//! `roots.log`, `node.log`, `GET /admin/status`, `POST /admin/chaos`,
//! `POST /exchange`, `POST /info`). Used by the harness's own tests.
//!
//! "Consensus": every `FAKE_BLOCK_MS` (default 100) a node commits the next
//! height iff its current chaos partition puts it in a group of ≥ quorum
//! nodes. Block ids / roots are a function of the height, so honest fake
//! nodes always agree. Test knobs (environment):
//! * `FAKE_RESUME_DELAY_MS` — after a heal, wait this long before committing.
//! * `FAKE_DIVERGE_NODE` + `FAKE_DIVERGE_HEIGHT` — that node writes a wrong
//!   root at that height and logs `STATE DIVERGENCE`.
//! * `FAKE_CONFLICT_NODE` + `FAKE_CONFLICT_HEIGHT` — wrong block id.

use chaos::http;
use serde_json::{json, Value};
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn env_u64(k: &str) -> Option<u64> {
    std::env::var(k).ok().and_then(|v| v.parse().ok())
}

fn wallet(seed: u64, tag: &str, i: usize) -> hlwire::Wallet {
    let mut ctr = 0u32;
    loop {
        let d = hlwire::keccak256(format!("fake-{tag}-{seed}-{i}-{ctr}").as_bytes());
        if let Ok(w) = hlwire::Wallet::from_bytes(&d) {
            return w;
        }
        ctr += 1;
    }
}

fn testnet_gen(args: &[String]) {
    let n: usize = arg(args, "--validators").and_then(|v| v.parse().ok()).unwrap_or(4);
    let out = PathBuf::from(arg(args, "--out").expect("--out"));
    let base_p2p: u16 = arg(args, "--base-p2p").and_then(|v| v.parse().ok()).unwrap_or(26600);
    let base_api: u16 = arg(args, "--base-api").and_then(|v| v.parse().ok()).unwrap_or(3001);
    let accounts: usize = arg(args, "--accounts").and_then(|v| v.parse().ok()).unwrap_or(0);
    let seed: u64 = arg(args, "--seed").and_then(|v| v.parse().ok()).unwrap_or(0);
    std::fs::create_dir_all(&out).unwrap();
    let mut vals = vec![];
    for i in 0..n {
        let w = wallet(seed, "val", i);
        let home = out.join(format!("node{i}"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("validator.key"), hex::encode(w.secret_bytes())).unwrap();
        std::fs::write(home.join("config.toml"), format!("api_listen = \"127.0.0.1:{}\"\n", base_api + i as u16)).unwrap();
        vals.push(json!({"address": w.address().to_hex(), "stake": 1, "p2p_addr": format!("127.0.0.1:{}", base_p2p + i as u16)}));
    }
    let g = json!({"chain_id": 1337, "is_mainnet": false, "genesis_time_ms": 0, "validators": {"validators": vals}});
    std::fs::write(out.join("genesis.json"), serde_json::to_vec_pretty(&g).unwrap()).unwrap();
    std::fs::write(out.join("validators.json"), serde_json::to_vec_pretty(&g["validators"]).unwrap()).unwrap();
    let accs: Vec<Value> = (0..accounts)
        .map(|i| {
            let w = wallet(seed, "acct", i);
            json!({"address": w.address().to_hex(), "secret": w.secret_hex()})
        })
        .collect();
    std::fs::write(out.join("accounts.json"), serde_json::to_vec_pretty(&accs).unwrap()).unwrap();
    println!("fake testnet with {n} validators in {}", out.display());
}

struct State {
    height: u64,
    groups: Vec<Vec<String>>,
    resume_at: Option<Instant>,
    txs: u64,
}

fn last_height(p: &Path) -> u64 {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| s.lines().filter_map(|l| l.split_whitespace().next()?.parse::<u64>().ok()).max())
        .unwrap_or(0)
}

fn hexh(tag: &str, h: u64) -> String {
    hex::encode(hlwire::keccak256(format!("{tag}{h}").as_bytes()))
}

fn append(p: &Path, s: &str) {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p).unwrap();
    f.write_all(s.as_bytes()).unwrap();
}

fn run(args: &[String]) {
    let home = PathBuf::from(arg(args, "--home").expect("--home"));
    let genesis = arg(args, "--genesis").map(PathBuf::from).unwrap_or_else(|| home.parent().unwrap().join("genesis.json"));
    let g: Value = serde_json::from_slice(&std::fs::read(&genesis).unwrap()).unwrap();
    let n = g["validators"]["validators"].as_array().unwrap().len();
    let quorum = 2 * n / 3 + 1;
    let key = std::fs::read_to_string(home.join("validator.key")).unwrap();
    let me = hlwire::Wallet::from_secret_hex(key.trim()).unwrap().address().to_hex();
    let idx: u64 = home.file_name().unwrap().to_string_lossy().trim_start_matches("node").parse().unwrap_or(0);
    let api = arg(args, "--api-listen").unwrap_or_else(|| "127.0.0.1:3001".into());
    let block_ms = env_u64("FAKE_BLOCK_MS").unwrap_or(100);
    let resume_delay = env_u64("FAKE_RESUME_DELAY_MS").unwrap_or(0);
    let commits = home.join("commits.log");
    let roots = home.join("roots.log");
    let nodelog = home.join("node.log");
    let st = Arc::new(Mutex::new(State { height: last_height(&commits), groups: vec![], resume_at: None, txs: 0 }));
    append(&nodelog, &format!("INFO fake node {me} starting at height {}\n", st.lock().unwrap().height));

    let st2 = st.clone();
    let me2 = me.clone();
    let listener = TcpListener::bind(&api).expect("bind api");
    std::thread::spawn(move || {
        http::serve(listener, move |req| {
            let mut s = st2.lock().unwrap();
            match (req.method.as_str(), req.path.as_str()) {
                ("GET", "/admin/status") => (
                    200,
                    json!({"validator": me2, "committed_height": s.height, "executed_height": s.height, "view": s.height,
                           "exec_root": format!("0x{}", hexh("root", s.height)), "mempool_len": 0, "peers": [], "last_commit_ms": 0})
                    .to_string(),
                ),
                ("POST", "/admin/chaos") => {
                    let v: Value = match serde_json::from_slice(&req.body) {
                        Ok(v) => v,
                        Err(e) => return (400, json!({"error": e.to_string()}).to_string()),
                    };
                    let groups: Vec<Vec<String>> = v
                        .get("partitions")
                        .and_then(|p| serde_json::from_value(p.clone()).ok())
                        .unwrap_or_default();
                    if !s.groups.is_empty() && groups.is_empty() && resume_delay > 0 {
                        s.resume_at = Some(Instant::now() + Duration::from_millis(resume_delay));
                    }
                    s.groups = groups;
                    (200, "{}".into())
                }
                ("POST", "/exchange") => {
                    s.txs += 1;
                    (200, json!({"status": "ok", "response": {"type": "default"}}).to_string())
                }
                ("POST", "/info") => (404, json!({"error": "fake node has no info"}).to_string()),
                _ => (404, "{}".into()),
            }
        })
    });

    let shared_path = genesis.with_file_name("fake-height");
    let div_node = env_u64("FAKE_DIVERGE_NODE");
    let div_h = env_u64("FAKE_DIVERGE_HEIGHT");
    let con_node = env_u64("FAKE_CONFLICT_NODE");
    let con_h = env_u64("FAKE_CONFLICT_HEIGHT");
    loop {
        std::thread::sleep(Duration::from_millis(block_ms));
        let mut s = st.lock().unwrap();
        if let Some(t) = s.resume_at {
            if Instant::now() < t {
                continue;
            }
            s.resume_at = None;
        }
        let in_quorum = s.groups.is_empty() || s.groups.iter().any(|g| g.len() >= quorum && g.iter().any(|a| a.eq_ignore_ascii_case(&me)));
        if !in_quorum {
            continue;
        }
        // "sync": catch up to the highest height any quorum member reached
        let shared = std::fs::read_to_string(&shared_path).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
        let target = s.height.max(shared) + 1;
        while s.height < target {
            s.height += 1;
            let h = s.height;
            let id = if con_node == Some(idx) && con_h == Some(h) { hexh("evil", h) } else { hexh("blk", h) };
            append(&commits, &format!("{h} 0x{id} {}\n", h * 100));
            let root = if div_node == Some(idx) && div_h == Some(h) {
                append(&nodelog, &format!("ERROR consensus: STATE DIVERGENCE at exec height {h}\n"));
                hexh("bad", h)
            } else {
                hexh("root", h)
            };
            append(&roots, &format!("{h} 0x{root} {}\n", s.txs));
            if h % 20 == 0 {
                println!("fake node committed {h}");
            }
        }
        if target > shared {
            let tmp = shared_path.with_extension(format!("tmp{idx}"));
            if std::fs::write(&tmp, target.to_string()).is_ok() {
                let _ = std::fs::rename(&tmp, &shared_path);
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("testnet-gen") => testnet_gen(&args),
        Some("run") => run(&args),
        _ => {
            eprintln!("usage: fake-hlnode testnet-gen|run ...");
            std::process::exit(2);
        }
    }
}
