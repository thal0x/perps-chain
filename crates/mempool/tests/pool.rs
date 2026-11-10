use interfaces::*;
use mempool::*;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn utx(i: u64) -> Tx {
    Tx::User(UserTx {
        action_bytes: i.to_le_bytes().repeat(8),
        nonce: i,
        signature: Sig65::default(),
        vault_address: None,
        expires_after: None,
    })
}

fn utx_sized(i: u64, n: usize) -> Tx {
    let mut b = vec![0u8; n];
    b[..8].copy_from_slice(&i.to_le_bytes());
    Tx::User(UserTx { action_bytes: b, nonce: i, signature: Sig65::default(), vault_address: None, expires_after: None })
}

fn vtx(i: u64) -> Tx {
    Tx::Validator(ValidatorTx {
        validator: Address([9; 20]),
        nonce: i,
        action: ValidatorAction::OraclePrices { prices: vec![(0, Px(i))] },
        signature: Sig65::default(),
    })
}

fn ok_checker() -> AdmissionChecker {
    Arc::new(|_| Ok(()))
}

fn api(ip: &str) -> TxSource {
    TxSource::Api { ip: ip.into() }
}

fn nolimit() -> PoolConfig {
    PoolConfig { ip_rate_per_s: 0, signer_rate_per_s: 0, gossip_interval_ms: 0, ..Default::default() }
}

#[derive(Default)]
struct FakeNet {
    sent: Mutex<Vec<NetMsg>>,
}
impl Network for FakeNet {
    fn local(&self) -> Address {
        Address([1; 20])
    }
    fn send(&self, _to: Address, msg: NetMsg) {
        self.sent.lock().push(msg);
    }
    fn broadcast(&self, msg: NetMsg) {
        self.sent.lock().push(msg);
    }
    fn peers(&self) -> Vec<Address> {
        vec![]
    }
}

fn gossiped(net: &FakeNet) -> Vec<Vec<Tx>> {
    net.sent
        .lock()
        .iter()
        .map(|m| match m {
            NetMsg::Txs(v) => v.clone(),
            _ => panic!("unexpected"),
        })
        .collect()
}

#[test]
fn dedup_including_recently_committed() {
    let clock = Arc::new(ManualClock::default());
    let pool = Pool::new(
        PoolConfig { recent_committed_ttl_ms: 1000, ..nolimit() },
        PoolDeps::new(ok_checker()).with_clock(clock.clone()),
    );
    let h = pool.insert(utx(1), api("a")).unwrap();
    assert_eq!(h, utx(1).hash());
    assert_eq!(pool.insert(utx(1), api("a")), Err(AdmissionError::Duplicate));
    assert_eq!(pool.insert(utx(1), TxSource::Peer(Address([2; 20]))), Err(AdmissionError::Duplicate));
    assert_eq!(pool.len(), 1);
    pool.remove_committed(&[h]);
    assert_eq!(pool.len(), 0);
    // re-gossip of a committed tx is rejected while remembered
    assert_eq!(pool.insert(utx(1), TxSource::Peer(Address([2; 20]))), Err(AdmissionError::Duplicate));
    // a hash committed elsewhere that we never had is remembered too
    pool.remove_committed(&[utx(2).hash()]);
    assert_eq!(pool.insert(utx(2), api("a")), Err(AdmissionError::Duplicate));
    clock.advance(1001);
    pool.prune();
    assert!(pool.insert(utx(1), api("a")).is_ok(), "forgotten after ttl");
    assert_eq!(pool.stats().duplicate, 4);
}

#[test]
fn checker_errors_propagate_and_are_not_admitted() {
    let checker: AdmissionChecker = Arc::new(|tx| match tx {
        Tx::User(u) if u.nonce % 2 == 1 => Err(AdmissionError::BadSignature),
        _ => Ok(()),
    });
    let pool = Pool::new(nolimit(), PoolDeps::new(checker));
    assert_eq!(pool.insert(utx(1), api("a")), Err(AdmissionError::BadSignature));
    assert!(pool.insert(utx(2), api("a")).is_ok());
    assert_eq!(pool.len(), 1);
    assert_eq!(pool.stats().check_failed, 1);
    // invalid tx can be retried later (not remembered as duplicate)
    assert_eq!(pool.insert(utx(1), api("a")), Err(AdmissionError::BadSignature));
}

#[test]
fn per_ip_rate_limit_with_deterministic_clock() {
    let clock = Arc::new(ManualClock::default());
    let cfg = PoolConfig { ip_rate_per_s: 2000, ip_burst: 4000, signer_rate_per_s: 0, gossip_interval_ms: 0, ..Default::default() };
    let pool = Pool::new(cfg, PoolDeps::new(ok_checker()).with_clock(clock.clone()));
    let mut i = 0u64;
    let mut burst = |ip: &str, n: usize| {
        (0..n)
            .filter(|_| {
                i += 1;
                pool.insert(utx(i), api(ip)).is_ok()
            })
            .count()
    };
    assert_eq!(burst("1.1.1.1", 5000), 4000, "burst");
    assert_eq!(burst("2.2.2.2", 10), 10, "other ip unaffected");
    clock.advance(500); // +1000 tokens
    assert_eq!(burst("1.1.1.1", 5000), 1000);
    clock.advance(1);
    assert_eq!(burst("1.1.1.1", 10), 2);
    // sustained 2000/s over 10 s
    let mut total = 0;
    for _ in 0..100 {
        clock.advance(100);
        total += burst("1.1.1.1", 1000);
    }
    assert_eq!(total, 20_000);
    // peer & local sources are not IP limited
    for k in 0..100 {
        pool.insert(utx(10_000_000 + k), TxSource::Peer(Address([3; 20]))).unwrap();
        pool.insert(utx(20_000_000 + k), TxSource::Local).unwrap();
    }
    assert!(pool.stats().rate_limited_ip > 0);
}

#[test]
fn per_signer_rate_limit_via_signer_of_and_prechecked() {
    let clock = Arc::new(ManualClock::default());
    let cfg = PoolConfig { ip_rate_per_s: 0, signer_rate_per_s: 10, signer_burst: 20, gossip_interval_ms: 0, ..Default::default() };
    // signer = nonce % 2
    let signer_of: SignerOf = Arc::new(|tx| match tx {
        Tx::User(u) => Some(Address([(u.nonce % 2) as u8; 20])),
        _ => None,
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let calls2 = calls.clone();
    let checker: AdmissionChecker = Arc::new(move |_| {
        calls2.fetch_add(1, Ordering::Relaxed);
        Ok(())
    });
    let pool = Pool::new(cfg, PoolDeps::new(checker).with_clock(clock.clone()).with_signer_of(signer_of));
    let ok_even = (0..100).filter(|k| pool.insert(utx(k * 2), api("x")).is_ok()).count();
    assert_eq!(ok_even, 20);
    let ok_odd = (0..100).filter(|k| pool.insert(utx(k * 2 + 1), api("x")).is_ok()).count();
    assert_eq!(ok_odd, 20);
    assert_eq!(pool.stats().rate_limited_signer, 160);
    // peer txs are not signer-limited by default
    assert!((1000..1050).all(|k| pool.insert(utx(k * 2), TxSource::Peer(Address([5; 20]))).is_ok()));
    // prechecked: signer passed explicitly, checker not called
    let before = calls.load(Ordering::Relaxed);
    let s = Address([7; 20]);
    let ok = (0..50).filter(|k| pool.insert_prechecked(utx(5000 + k), api("x"), Some(s)).is_ok()).count();
    assert_eq!(ok, 20);
    assert_eq!(calls.load(Ordering::Relaxed), before);
    clock.advance(1000);
    let ok = (50..100).filter(|k| pool.insert_prechecked(utx(5000 + k), api("x"), Some(s)).is_ok()).count();
    assert_eq!(ok, 10);
}

#[test]
fn capacity_rejects_user_and_evicts_for_validator_txs() {
    let pool = Pool::new(PoolConfig { max_txs: 100, ..nolimit() }, PoolDeps::new(ok_checker()));
    for i in 0..100 {
        pool.insert(utx(i), api("a")).unwrap();
    }
    assert_eq!(pool.insert(utx(1000), api("a")), Err(AdmissionError::MempoolFull));
    assert_eq!(pool.insert(utx(1001), TxSource::Peer(Address([2; 20]))), Err(AdmissionError::MempoolFull));
    // validator txs evict the oldest user txs
    for v in 0..5 {
        pool.insert(vtx(v), TxSource::Local).unwrap();
    }
    assert_eq!(pool.len(), 100);
    assert_eq!(pool.stats().evicted_for_validator_txs, 5);
    for i in 0..5 {
        assert!(!pool.contains(&utx(i).hash()), "oldest {i} evicted");
    }
    assert!(pool.contains(&utx(5).hash()));
    assert_eq!(pool.count_slow().0, 100);

    // byte cap
    let one = utx_sized(0, 1000).encoded_len();
    let pool = Pool::new(PoolConfig { max_bytes: one * 10, ..nolimit() }, PoolDeps::new(ok_checker()));
    for i in 0..10 {
        pool.insert(utx_sized(i, 1000), api("a")).unwrap();
    }
    assert_eq!(pool.insert(utx_sized(10, 1000), api("a")), Err(AdmissionError::MempoolFull));
    assert_eq!(pool.bytes(), one * 10);
    pool.remove_committed(&[utx_sized(0, 1000).hash()]);
    assert!(pool.insert(utx_sized(10, 1000), api("a")).is_ok());
}

#[test]
fn reap_order_limits_and_non_removal() {
    let pool = Pool::new(nolimit(), PoolDeps::new(ok_checker()));
    for i in 0..1000 {
        pool.insert(utx(i), api("a")).unwrap();
        if i % 100 == 50 {
            pool.insert(vtx(i), TxSource::Local).unwrap();
        }
    }
    let r = pool.reap(10_000, usize::MAX);
    assert_eq!(r.len(), 1010);
    let mut want: Vec<Tx> = (0..1000).filter(|i| i % 100 == 50).map(vtx).collect();
    want.extend((0..1000).map(utx));
    assert_eq!(r, want, "validator txs first, then FIFO");
    // not removed
    assert_eq!(pool.len(), 1010);
    assert_eq!(pool.reap(10_000, usize::MAX), want);
    // max_txs
    assert_eq!(pool.reap(15, usize::MAX), want[..15].to_vec());
    // max_bytes
    let sz = utx(0).encoded_len();
    let vsz = vtx(0).encoded_len();
    let r = pool.reap(10_000, vsz * 10 + sz * 7);
    assert_eq!(r, want[..17].to_vec());
    assert_eq!(pool.reap(0, usize::MAX), vec![]);
    assert_eq!(pool.reap(10, 0), vec![]);
}

#[test]
fn reap_skips_oversized_tx() {
    let pool = Pool::new(nolimit(), PoolDeps::new(ok_checker()));
    pool.insert(utx_sized(0, 100), api("a")).unwrap();
    pool.insert(utx_sized(1, 10_000), api("a")).unwrap();
    pool.insert(utx_sized(2, 100), api("a")).unwrap();
    let r = pool.reap(10, 1000);
    assert_eq!(r, vec![utx_sized(0, 100), utx_sized(2, 100)]);
}

#[test]
fn removal_ttl_and_recheck() {
    let clock = Arc::new(ManualClock::default());
    let pool = Pool::new(PoolConfig { tx_ttl_ms: 1000, ..nolimit() }, PoolDeps::new(ok_checker()).with_clock(clock.clone()));
    let hs: Vec<TxHash> = (0..100).map(|i| pool.insert(utx(i), api("a")).unwrap()).collect();
    pool.remove_committed(&hs[10..20]);
    assert_eq!(pool.len(), 90);
    let r = pool.reap(1000, usize::MAX);
    assert_eq!(r, (0..100).filter(|i| !(10..20).contains(i)).map(utx).collect::<Vec<_>>());
    // removing unknown / already-removed hashes is harmless
    pool.remove_committed(&hs[10..20]);
    pool.remove_committed(&[Hash32([0xee; 32])]);
    assert_eq!(pool.len(), 90);
    assert_eq!(pool.count_slow(), (90, pool.bytes()));
    // retain (e.g. invalidated nonces)
    assert_eq!(pool.retain(|tx| matches!(tx, Tx::User(u) if u.nonce >= 50)), 40);
    assert_eq!(pool.len(), 50);
    // ttl expiry
    clock.advance(500);
    for i in 200..210 {
        pool.insert(utx(i), api("a")).unwrap();
    }
    clock.advance(600);
    pool.prune();
    assert_eq!(pool.len(), 10, "first batch expired, second kept");
    assert_eq!(pool.stats().expired, 50);
    assert_eq!(pool.count_slow(), (10, pool.bytes()));
}

#[test]
fn recheck_uses_current_checker_state() {
    let bad_below = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let b2 = bad_below.clone();
    let checker: AdmissionChecker = Arc::new(move |tx| match tx {
        Tx::User(u) if u.nonce < b2.load(Ordering::Relaxed) => Err(AdmissionError::BadNonce("old".into())),
        _ => Ok(()),
    });
    let pool = Pool::new(nolimit(), PoolDeps::new(checker));
    for i in 0..100 {
        pool.insert(utx(i), api("a")).unwrap();
    }
    bad_below.store(30, Ordering::Relaxed);
    assert_eq!(pool.recheck(), 30);
    assert_eq!(pool.len(), 70);
}

#[test]
fn gossip_batches_local_txs_only() {
    let net = Arc::new(FakeNet::default());
    let pool = Pool::new(
        PoolConfig { gossip_max_batch_txs: 4, ..nolimit() },
        PoolDeps::new(ok_checker()).with_network(net.clone()),
    );
    for i in 0..10 {
        pool.insert(utx(i), api("a")).unwrap();
    }
    pool.insert(vtx(0), TxSource::Local).unwrap();
    let r = pool.handle_peer_txs(Address([2; 20]), vec![utx(100), utx(101), utx(1)]);
    assert_eq!(r, PeerBatchResult { admitted: 2, duplicate: 1, rejected: 0 });
    // rejected local txs are not gossiped
    assert!(pool.insert(utx(1), api("a")).is_err());
    assert!(net.sent.lock().is_empty(), "nothing sent before flush");
    pool.flush_gossip();
    let b = gossiped(&net);
    assert_eq!(b.iter().map(|v| v.len()).collect::<Vec<_>>(), vec![4, 4, 3]);
    let flat: Vec<Tx> = b.into_iter().flatten().collect();
    let mut want: Vec<Tx> = (0..10).map(utx).collect();
    want.push(vtx(0));
    assert_eq!(flat, want, "peer txs are not re-gossiped");
    pool.flush_gossip();
    assert_eq!(net.sent.lock().len(), 3, "empty flush sends nothing");
    assert_eq!(pool.len(), 13);
    let s = pool.stats();
    assert_eq!((s.gossiped_txs, s.gossip_batches, s.admitted_from_peers), (11, 3, 2));
}

#[test]
fn gossip_byte_batching() {
    let net = Arc::new(FakeNet::default());
    let one = utx_sized(0, 1000).encoded_len();
    let pool = Pool::new(
        PoolConfig { gossip_max_batch_bytes: one * 3, ..nolimit() },
        PoolDeps::new(ok_checker()).with_network(net.clone()),
    );
    for i in 0..7 {
        pool.insert(utx_sized(i, 1000), api("a")).unwrap();
    }
    pool.flush_gossip();
    assert_eq!(gossiped(&net).iter().map(|v| v.len()).collect::<Vec<_>>(), vec![3, 3, 1]);
}

#[test]
fn gossip_timer_thread_flushes_every_interval() {
    let net = Arc::new(FakeNet::default());
    let pool = Pool::new(
        PoolConfig { gossip_interval_ms: 10, ..nolimit() },
        PoolDeps::new(ok_checker()).with_network(net.clone()),
    );
    for i in 0..5 {
        pool.insert(utx(i), api("a")).unwrap();
    }
    let t0 = Instant::now();
    while net.sent.lock().is_empty() {
        assert!(t0.elapsed() < Duration::from_secs(2), "timer flush");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(gossiped(&net).concat(), (0..5).map(utx).collect::<Vec<_>>());
    drop(pool); // thread exits on its own
}

#[test]
fn concurrency_stress() {
    let net = Arc::new(FakeNet::default());
    let pool = Pool::new(
        PoolConfig { max_txs: 50_000, gossip_interval_ms: 1, ..nolimit() },
        PoolDeps::new(ok_checker()).with_network(net.clone()),
    );
    let threads = 8;
    let per = 20_000u64;
    let committed = Arc::new(Mutex::new(Vec::<TxHash>::new()));
    std::thread::scope(|s| {
        for t in 0..threads {
            let pool = pool.clone();
            s.spawn(move || {
                // overlapping ranges -> many duplicates across threads
                let base = (t as u64 / 2) * per;
                for chunk in (base..base + per).collect::<Vec<_>>().chunks(100) {
                    let src = if t % 2 == 0 { api(&format!("ip{t}")) } else { TxSource::Peer(Address([t as u8; 20])) };
                    let _ = pool.insert_batch(chunk.iter().map(|i| utx(*i)).collect(), src);
                }
            });
        }
        // concurrent reaper / committer
        let pool2 = pool.clone();
        let committed = committed.clone();
        s.spawn(move || {
            for _ in 0..200 {
                let r = pool2.reap(500, 1 << 20);
                let hs: Vec<TxHash> = r.iter().map(|t| t.hash()).collect();
                pool2.remove_committed(&hs);
                committed.lock().extend(hs);
                std::thread::sleep(Duration::from_micros(200));
            }
        });
    });
    pool.flush_gossip();
    let (n, b) = pool.count_slow();
    assert_eq!(n, pool.len());
    assert_eq!(b, pool.bytes());
    assert!(n <= 50_000);
    let committed = committed.lock().clone();
    let mut uniq = committed.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), committed.len(), "a tx was reaped after being committed");
    for h in &committed {
        assert!(!pool.contains(h));
    }
    // everything gossiped was admitted from API exactly once
    let gossip: Vec<TxHash> = gossiped(&net).concat().iter().map(|t| t.hash()).collect();
    let mut g = gossip.clone();
    g.sort();
    g.dedup();
    assert_eq!(g.len(), gossip.len(), "gossiped twice");
    let s = pool.stats();
    assert_eq!(s.admitted as usize, n + committed.len() + s.evicted_for_validator_txs as usize + s.expired as usize);
}

/// Perf targets: >= 300k inserts/s on one thread (trivial checker), reap of
/// 20k txs < 5 ms. Run: `cargo test -p mempool --release -- --ignored perf --nocapture`.
#[test]
#[ignore]
fn perf_insert_and_reap() {
    let pool = Pool::new(nolimit(), PoolDeps::new(ok_checker()));
    let n = 200_000u64;
    let txs: Vec<Tx> = (0..n).map(|i| utx_sized(i, 200)).collect();
    let t0 = Instant::now();
    for tx in txs {
        pool.insert(tx, TxSource::Local).unwrap();
    }
    let el = t0.elapsed();
    let rate = n as f64 / el.as_secs_f64();
    let mut best = Duration::MAX;
    for _ in 0..10 {
        let t = Instant::now();
        let r = pool.reap(20_000, usize::MAX);
        best = best.min(t.elapsed());
        assert_eq!(r.len(), 20_000);
    }
    // multi-thread insert
    let pool2 = Pool::new(nolimit(), PoolDeps::new(ok_checker()));
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for t in 0..4u64 {
            let pool2 = pool2.clone();
            s.spawn(move || {
                let txs: Vec<Tx> = (0..n / 4).map(|i| utx_sized(t * n + i, 200)).collect();
                pool2.insert_batch(txs, TxSource::Local);
            });
        }
    });
    let mt = n as f64 / t0.elapsed().as_secs_f64();
    println!("insert: {rate:.0}/s (1 thread), {mt:.0}/s (4 threads); reap 20k of 200k: {best:?}");
    assert!(rate >= 300_000.0, "insert rate {rate:.0}/s");
    assert!(best < Duration::from_millis(5), "reap {best:?}");
}
