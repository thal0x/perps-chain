//! Background load: either an external `sim load` process, or a built-in
//! light generator posting real signed HL actions to `/exchange` so blocks
//! carry transactions (resting ALO orders well below mid, then cancelling
//! them by cloid; `noop` actions if no market can be discovered).

use crate::http;
use interfaces::{Grouping, OrderKind, OrderRequest, Tif};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Default, Debug)]
pub struct LoadStats {
    pub sent: AtomicU64,
    pub accepted: AtomicU64,
    pub rejected: AtomicU64,
    pub http_errors: AtomicU64,
    pub orders: AtomicU64,
    pub cancels: AtomicU64,
    pub noops: AtomicU64,
    /// Ticks skipped because too many requests were in flight.
    pub skipped: AtomicU64,
    pub inflight: AtomicU64,
}

/// Max concurrent `/exchange` requests (the endpoint answers after the tx
/// is executed, so requests block while the chain is halted).
pub const MAX_INFLIGHT: u64 = 32;

impl LoadStats {
    pub fn snapshot(&self) -> BTreeMap<String, u64> {
        let mut m = BTreeMap::new();
        for (k, v) in [
            ("sent", &self.sent),
            ("accepted", &self.accepted),
            ("rejected", &self.rejected),
            ("http_errors", &self.http_errors),
            ("orders", &self.orders),
            ("cancels", &self.cancels),
            ("noops", &self.noops),
            ("skipped", &self.skipped),
        ] {
            m.insert(k.to_string(), v.load(Ordering::Relaxed));
        }
        m
    }
}

/// Read `accounts.json` (`[{address, secret}]`).
pub fn read_accounts(path: &Path) -> Vec<hlwire::Wallet> {
    let Ok(b) = std::fs::read(path) else { return vec![] };
    let Ok(v) = serde_json::from_slice::<Value>(&b) else { return vec![] };
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("secret").and_then(|s| s.as_str()))
                .filter_map(|s| hlwire::Wallet::from_secret_hex(s).ok())
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Market {
    pub asset: u32,
    pub name: String,
    pub sz_decimals: u32,
    pub mid: f64,
}

/// Format a price with ≤ 5 significant figures and ≤ 6 - szDecimals decimals.
pub fn format_px(px: f64, sz_decimals: u32) -> String {
    let px = px.max(1e-6);
    let digits = px.log10().floor() as i32 + 1;
    let max_dec = 6i32 - sz_decimals as i32;
    let dec = (5 - digits).clamp(0, max_dec.max(0)) as usize;
    let s = format!("{:.*}", dec, px);
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        // integer part with > 5 digits: zero out below 5 sig figs
        let v: u64 = s.parse().unwrap_or(1);
        let excess = (digits - 5).max(0) as u32;
        let m = 10u64.pow(excess);
        ((v / m) * m).max(1).to_string()
    }
}

pub fn format_sz(notional: f64, px: f64, sz_decimals: u32) -> String {
    let scale = 10f64.powi(sz_decimals as i32);
    let sz = ((notional / px) * scale).ceil().max(1.0) / scale;
    let s = format!("{:.*}", sz_decimals as usize, sz);
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

fn info(api: &str, body: Value) -> Option<Value> {
    let r = http::post_json(api, "/info", &body.to_string(), Duration::from_secs(2)).ok()?;
    if !r.ok() {
        return None;
    }
    serde_json::from_slice(&r.body).ok()
}

pub fn discover_market(api: &str) -> Option<Market> {
    let meta = info(api, json!({"type": "meta"}))?;
    let u = meta.get("universe")?.as_array()?;
    let first = u.first()?;
    let name = first.get("name")?.as_str()?.to_string();
    let sz_decimals = first.get("szDecimals").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    let mids = info(api, json!({"type": "allMids"}))?;
    let mid: f64 = mids.get(&name)?.as_str()?.parse().ok()?;
    if mid <= 0.0 {
        return None;
    }
    Some(Market { asset: 0, name, sz_decimals, mid })
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub struct BuiltinLoad {
    pub apis: Vec<String>,
    pub alive: Arc<Vec<AtomicBool>>,
    pub wallets: Vec<hlwire::Wallet>,
    pub is_mainnet: bool,
    pub rate: f64,
    pub seed: u64,
}

impl BuiltinLoad {
    /// Run on a background thread until `stop`.
    pub fn spawn(self, stop: Arc<AtomicBool>) -> Arc<LoadStats> {
        let stats = Arc::new(LoadStats::default());
        let st = stats.clone();
        std::thread::Builder::new().name("load".into()).spawn(move || self.run(st, &stop)).expect("spawn load");
        stats
    }

    fn pick_api(&self, rng: &mut ChaCha8Rng) -> Option<&str> {
        let live: Vec<usize> = (0..self.apis.len()).filter(|i| self.alive[*i].load(Ordering::Relaxed)).collect();
        if live.is_empty() {
            return None;
        }
        Some(&self.apis[live[rng.gen_range(0..live.len())]])
    }

    fn run(self, st: Arc<LoadStats>, stop: &AtomicBool) {
        if self.wallets.is_empty() || self.rate <= 0.0 {
            return;
        }
        let mut rng = ChaCha8Rng::seed_from_u64(self.seed ^ 0x10ad);
        // market discovery (retry for a while; nodes may still be starting)
        let mut market = None;
        let t0 = Instant::now();
        while market.is_none() && t0.elapsed() < Duration::from_secs(15) && !stop.load(Ordering::Relaxed) {
            if let Some(api) = self.pick_api(&mut rng) {
                market = discover_market(api);
            }
            if market.is_none() {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        let period = Duration::from_secs_f64(1.0 / self.rate);
        let mut last_nonce: Vec<u64> = vec![0; self.wallets.len()];
        let mut outstanding: Vec<Option<[u8; 16]>> = vec![None; self.wallets.len()];
        let mut k = 0usize;
        let mut next = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now < next {
                std::thread::sleep((next - now).min(Duration::from_millis(200)));
                continue;
            }
            next += period;
            if next + Duration::from_secs(2) < Instant::now() {
                next = Instant::now(); // don't burst after a stall
            }
            if st.inflight.load(Ordering::SeqCst) >= MAX_INFLIGHT {
                st.skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let w = k % self.wallets.len();
            k += 1;
            let nonce = now_ms().max(last_nonce[w] + 1);
            last_nonce[w] = nonce;
            let action = match &market {
                Some(m) => match outstanding[w].take() {
                    Some(cloid) => {
                        st.cancels.fetch_add(1, Ordering::Relaxed);
                        hlwire::cancel_by_cloid_action(&[(m.asset, cloid)])
                    }
                    None => {
                        let frac = 0.80 + rng.gen_range(0..10) as f64 / 100.0;
                        let px_s = format_px(m.mid * frac, m.sz_decimals);
                        let px_f: f64 = px_s.parse().unwrap_or(m.mid);
                        let sz_s = format_sz(15.0, px_f, m.sz_decimals);
                        let (Ok(px), Ok(sz)) = (interfaces::Px::parse(&px_s), interfaces::Sz::parse(&sz_s)) else {
                            continue;
                        };
                        let cloid: [u8; 16] = rng.gen();
                        outstanding[w] = Some(cloid);
                        st.orders.fetch_add(1, Ordering::Relaxed);
                        let o = OrderRequest {
                            asset: m.asset,
                            is_buy: true,
                            limit_px: px,
                            sz,
                            reduce_only: false,
                            kind: OrderKind::Limit { tif: Tif::Alo },
                            cloid: Some(cloid),
                        };
                        hlwire::order_action(&[o], Grouping::Na)
                    }
                },
                None => {
                    st.noops.fetch_add(1, Ordering::Relaxed);
                    hlwire::noop_action()
                }
            };
            let body = hlwire::signed_l1_request(&self.wallets[w], action, nonce, None, None, self.is_mainnet);
            let Some(api) = self.pick_api(&mut rng).map(|s| s.to_string()) else { continue };
            st.sent.fetch_add(1, Ordering::Relaxed);
            st.inflight.fetch_add(1, Ordering::SeqCst);
            let st2 = st.clone();
            std::thread::spawn(move || {
                match http::post_json(&api, "/exchange", &body.to_string(), Duration::from_secs(15)) {
                    Ok(r) if r.ok() && r.text().contains("\"ok\"") => {
                        st2.accepted.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(_) => {
                        st2.rejected.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        st2.http_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
                st2.inflight.fetch_sub(1, Ordering::SeqCst);
            });
        }
    }
}

/// Spawn an external load generator from a template
/// (`{sim}`, `{rate}`, `{target}` = comma-separated `http://` URLs,
/// `{targets_args}` = `--target URL` repeated).
pub fn spawn_sim(sim: &Path, template: &str, rate: f64, apis: &[String], log: &Path) -> std::io::Result<Child> {
    let urls: Vec<String> = apis.iter().map(|a| format!("http://{a}")).collect();
    let mut args = vec![];
    for tok in template.split_whitespace() {
        if tok == "{targets_args}" {
            for u in &urls {
                args.push("--target".to_string());
                args.push(u.clone());
            }
            continue;
        }
        args.push(tok.replace("{rate}", &format!("{rate}")).replace("{target}", &urls.join(",")));
    }
    let f = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let mut cmd = Command::new(sim);
    cmd.args(&args).stdin(Stdio::null()).stdout(f.try_clone()?).stderr(f);
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    cmd.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn px_formatting() {
        assert_eq!(format_px(45123.456, 5), "45123");
        assert_eq!(format_px(123456.7, 2), "123450");
        assert_eq!(format_px(3.14159, 2), "3.1416");
        assert_eq!(format_px(0.012345678, 0), "0.012346");
        assert_eq!(format_px(0.012345678, 4), "0.01");
        assert!(interfaces::Px::parse(&format_px(2999.99, 4)).is_ok());
    }

    #[test]
    fn sz_formatting() {
        assert_eq!(format_sz(15.0, 45000.0, 5), "0.00034");
        assert_eq!(format_sz(15.0, 3.0, 0), "5");
        assert_eq!(format_sz(15.0, 1e9, 2), "0.01");
    }

    #[test]
    fn builtin_load_signs_and_posts() {
        // capture /exchange bodies with a tiny server
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let seen = Arc::new(std::sync::Mutex::new(vec![]));
        let s2 = seen.clone();
        std::thread::spawn(move || {
            http::serve(l, move |req| {
                if req.path == "/exchange" {
                    s2.lock().unwrap().push(String::from_utf8_lossy(&req.body).into_owned());
                    (200, r#"{"status":"ok","response":{"type":"default"}}"#.into())
                } else if req.path == "/info" {
                    let b = String::from_utf8_lossy(&req.body).into_owned();
                    if b.contains("allMids") {
                        (200, r#"{"BTC":"50000.0"}"#.into())
                    } else {
                        (200, r#"{"universe":[{"name":"BTC","szDecimals":5,"maxLeverage":50}]}"#.into())
                    }
                } else {
                    (404, "{}".into())
                }
            })
        });
        let mut rng = ChaCha8Rng::seed_from_u64(1);
        let wallets = vec![hlwire::Wallet::random(&mut rng), hlwire::Wallet::random(&mut rng)];
        let stop = Arc::new(AtomicBool::new(false));
        let stats = BuiltinLoad {
            apis: vec![addr],
            alive: Arc::new(vec![AtomicBool::new(true)]),
            wallets,
            is_mainnet: false,
            rate: 50.0,
            seed: 3,
        }
        .spawn(stop.clone());
        let t0 = Instant::now();
        while stats.accepted.load(Ordering::Relaxed) < 8 && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::Relaxed);
        assert!(stats.accepted.load(Ordering::Relaxed) >= 8);
        assert!(stats.orders.load(Ordering::Relaxed) >= 2);
        assert!(stats.cancels.load(Ordering::Relaxed) >= 2);
        let bodies = seen.lock().unwrap().clone();
        for b in bodies.iter().take(8) {
            let v: Value = serde_json::from_str(b).unwrap();
            let tx = hlwire::parse_exchange_request(&v).expect("valid exchange request");
            let _ = tx;
        }
        assert!(bodies.iter().any(|b| b.contains("\"order\"")));
        assert!(bodies.iter().any(|b| b.contains("cancelByCloid")));
    }

    #[test]
    fn sim_template_expansion() {
        let d = crate::testutil::tempdir("simtpl");
        let mut c = spawn_sim(Path::new("/bin/echo"), "load --rps {rate} --target {target} {targets_args}", 5.0, &["127.0.0.1:1".into(), "127.0.0.1:2".into()], &d.join("sim.log")).unwrap();
        c.wait().unwrap();
        let out = std::fs::read_to_string(d.join("sim.log")).unwrap();
        assert_eq!(out.trim(), "load --rps 5 --target http://127.0.0.1:1,http://127.0.0.1:2 --target http://127.0.0.1:1 --target http://127.0.0.1:2");
    }
}
