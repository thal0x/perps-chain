//! Localhost integration tests for `net::P2p`.

use crossbeam_channel::Receiver;
use interfaces::*;
use net::handshake::{handshake, handshake_claiming, Identity};
use net::{ChaosConfig, Inbound, LinkChaos, NetConfig, P2p};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CHAIN: u64 = 77;

fn key(i: usize) -> [u8; 32] {
    [(i as u8) + 1; 32]
}

struct Cluster {
    cfgs: Vec<NetConfig>,
    /// Sorted by address (index 0 = lowest = dials everyone).
    addrs: Vec<Address>,
    /// Index by position in `addrs`.
    nodes: Vec<Option<(Arc<P2p>, Inbound)>>,
}

impl Cluster {
    fn new(n: usize, tweak: impl Fn(&mut NetConfig)) -> Self {
        let listeners: Vec<std::net::TcpListener> =
            (0..n).map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap()).collect();
        let mut ids: Vec<(Identity, String, usize)> = (0..n)
            .map(|i| {
                let id = Identity::new(&key(i), CHAIN).unwrap();
                (id, listeners[i].local_addr().unwrap().to_string(), i)
            })
            .collect();
        ids.sort_by_key(|(id, _, _)| id.address);
        let vs = ValidatorSet {
            validators: ids
                .iter()
                .map(|(id, a, _)| ValidatorInfo { address: id.address, stake: 1, p2p_addr: a.clone() })
                .collect(),
        };
        let mut listeners: Vec<Option<std::net::TcpListener>> = listeners.into_iter().map(Some).collect();
        let mut cfgs = vec![];
        let mut nodes = vec![];
        for (_, _, i) in &ids {
            let mut cfg = NetConfig {
                chain_id: CHAIN,
                secret_key: key(*i),
                validators: vs.clone(),
                backoff_min_ms: 20,
                backoff_max_ms: 200,
                ..Default::default()
            };
            tweak(&mut cfg);
            let (p, inb) = P2p::start_with_listener(cfg.clone(), listeners[*i].take().unwrap()).unwrap();
            cfgs.push(cfg);
            nodes.push(Some((p, inb)));
        }
        let addrs = ids.iter().map(|(id, _, _)| id.address).collect();
        Self { cfgs, addrs, nodes }
    }

    fn p(&self, i: usize) -> &Arc<P2p> {
        &self.nodes[i].as_ref().unwrap().0
    }
    fn rx(&self, i: usize) -> &Inbound {
        &self.nodes[i].as_ref().unwrap().1
    }

    fn wait_full_mesh(&self) {
        wait_until(Duration::from_secs(10), || {
            self.nodes.iter().flatten().all(|(p, _)| p.peers().len() == self.addrs.len() - 1)
        })
        .expect("full mesh");
    }

    fn stop(&mut self, i: usize) {
        let (p, _) = self.nodes[i].take().unwrap();
        p.shutdown();
    }

    fn restart(&mut self, i: usize) {
        let addr = self.cfgs[i].validators.validators[i].p2p_addr.clone();
        // The listener port may linger briefly; retry the bind.
        let mut l = None;
        for _ in 0..100 {
            match std::net::TcpListener::bind(&addr) {
                Ok(x) => {
                    l = Some(x);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        let (p, inb) = P2p::start_with_listener(self.cfgs[i].clone(), l.expect("rebind")).unwrap();
        self.nodes[i] = Some((p, inb));
    }
}

fn wait_until(t: Duration, mut f: impl FnMut() -> bool) -> Result<(), ()> {
    let end = Instant::now() + t;
    while Instant::now() < end {
        if f() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if f() {
        Ok(())
    } else {
        Err(())
    }
}

fn status(h: u64) -> NetMsg {
    NetMsg::Consensus(ConsensusMsg::Status { committed_height: h, high_qc_view: 0 })
}

fn utx(i: u64, size: usize) -> Tx {
    Tx::User(UserTx {
        action_bytes: vec![(i % 251) as u8; size],
        nonce: i,
        signature: Sig65::default(),
        vault_address: None,
        expires_after: None,
    })
}

fn recv_heights(rx: &Receiver<(Address, NetMsg)>, n: usize, t: Duration) -> Vec<(Address, u64)> {
    let mut out = vec![];
    let end = Instant::now() + t;
    while out.len() < n {
        let left = end.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((from, NetMsg::Consensus(ConsensusMsg::Status { committed_height, .. }))) => out.push((from, committed_height)),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out
}

fn drain(rx: &Receiver<(Address, NetMsg)>) {
    while rx.try_recv().is_ok() {}
}

#[test]
fn four_node_mesh_broadcast_send_loopback() {
    let c = Cluster::new(4, |_| {});
    c.wait_full_mesh();
    for i in 0..4 {
        let mut peers = c.p(i).peers();
        peers.sort();
        let mut want: Vec<Address> = c.addrs.iter().copied().filter(|a| *a != c.addrs[i]).collect();
        want.sort();
        assert_eq!(peers, want);
        assert_eq!(c.p(i).local(), c.addrs[i]);
    }
    for i in 0..4 {
        c.p(i).broadcast(status(i as u64 + 100));
    }
    for i in 0..4 {
        let mut got = recv_heights(&c.rx(i).consensus, 3, Duration::from_secs(5));
        got.sort();
        let mut want: Vec<(Address, u64)> =
            (0..4).filter(|j| *j != i).map(|j| (c.addrs[j], j as u64 + 100)).collect();
        want.sort();
        assert_eq!(got, want, "node {i}");
    }
    // point-to-point
    c.p(0).send(c.addrs[3], status(7));
    assert_eq!(recv_heights(&c.rx(3).consensus, 1, Duration::from_secs(5)), vec![(c.addrs[0], 7)]);
    // loopback
    c.p(2).send(c.addrs[2], status(8));
    assert_eq!(recv_heights(&c.rx(2).consensus, 1, Duration::from_secs(1)), vec![(c.addrs[2], 8)]);
    // txs go to the tx channel, not the consensus channel
    c.p(1).broadcast(NetMsg::Txs(vec![utx(1, 10), utx(2, 20)]));
    for j in [0, 2, 3] {
        let (from, m) = c.rx(j).txs.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(from, c.addrs[1]);
        assert_eq!(m, NetMsg::Txs(vec![utx(1, 10), utx(2, 20)]));
        assert!(c.rx(j).consensus.try_recv().is_err());
    }
    // per-peer FIFO order on one link
    for h in 0..500 {
        c.p(3).send(c.addrs[1], status(h));
    }
    let got = recv_heights(&c.rx(1).consensus, 500, Duration::from_secs(5));
    assert_eq!(got.iter().map(|x| x.1).collect::<Vec<_>>(), (0..500).collect::<Vec<_>>());
    let m = c.p(3).metrics();
    let m1 = &m[&c.addrs[1]];
    assert!(m1.connected && m1.msgs_out >= 501 && m1.bytes_out > 0, "{m1:?}");
    let m = c.p(1).metrics();
    assert!(m[&c.addrs[3]].msgs_in >= 501 && m[&c.addrs[3]].bytes_in > 0);
}

#[test]
fn reconnect_after_peer_restarts() {
    let mut c = Cluster::new(4, |_| {});
    c.wait_full_mesh();
    // Restart the lowest (pure dialer) and the highest (pure acceptor).
    for victim in [0usize, 3] {
        c.stop(victim);
        let others: Vec<usize> = (0..4).filter(|j| *j != victim).collect();
        wait_until(Duration::from_secs(5), || others.iter().all(|j| !c.p(*j).peers().contains(&c.addrs[victim])))
            .expect("peers notice disconnect");
        // Sends to a down peer are dropped, not queued forever.
        c.p(others[0]).send(c.addrs[victim], status(1));
        c.restart(victim);
        c.wait_full_mesh();
        for j in 0..4 {
            drain(&c.rx(j).consensus);
        }
        c.p(victim).broadcast(status(42));
        for j in &others {
            assert_eq!(recv_heights(&c.rx(*j).consensus, 1, Duration::from_secs(5)), vec![(c.addrs[victim], 42)]);
            c.p(*j).send(c.addrs[victim], status(*j as u64));
        }
        let mut got = recv_heights(&c.rx(victim).consensus, 3, Duration::from_secs(5));
        got.sort();
        let mut want: Vec<(Address, u64)> = others.iter().map(|j| (c.addrs[*j], *j as u64)).collect();
        want.sort();
        assert_eq!(got, want);
        let m = c.p(others[0]).metrics();
        assert!(m[&c.addrs[victim]].connects >= 2);
    }
}

#[test]
fn handshake_rejects_wrong_key_non_validator_and_wrong_chain() {
    let c = Cluster::new(4, |_| {});
    c.wait_full_mesh();
    let target = c.cfgs[3].validators.validators[3].p2p_addr.clone();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        use tokio::io::AsyncReadExt;
        // 1. claims validator 0's address, signs with a different key.
        let impostor = Identity::new(&[99u8; 32], CHAIN).unwrap();
        let mut s = tokio::net::TcpStream::connect(&target).await.unwrap();
        let _ = handshake_claiming(&mut s, &impostor, c.addrs[0]).await;
        let mut b = [0u8; 1];
        let r = tokio::time::timeout(Duration::from_secs(5), s.read(&mut b)).await.unwrap();
        assert!(matches!(r, Ok(0) | Err(_)), "connection must be closed, got {r:?}");

        // 2. honest non-validator.
        let mut s = tokio::net::TcpStream::connect(&target).await.unwrap();
        let r = tokio::time::timeout(Duration::from_secs(5), handshake(&mut s, &impostor, |_| true)).await.unwrap();
        assert!(r.is_err(), "non-validator must be refused");

        // 3. real validator key, wrong chain id.
        let wrong_chain = Identity::new(&key(0), CHAIN + 1).unwrap();
        let mut s = tokio::net::TcpStream::connect(&target).await.unwrap();
        let r = tokio::time::timeout(Duration::from_secs(5), handshake(&mut s, &wrong_chain, |_| true)).await.unwrap();
        assert!(r.is_err());
    });
    // The genuine connection 0 <-> 3 is unaffected.
    assert!(c.p(3).peers().contains(&c.addrs[0]));
    c.p(0).send(c.addrs[3], status(5));
    let got = recv_heights(&c.rx(3).consensus, 1, Duration::from_secs(5));
    assert_eq!(got, vec![(c.addrs[0], 5)]);
}

#[test]
fn chaos_partition_blocks_and_heals() {
    let c = Cluster::new(4, |_| {});
    c.wait_full_mesh();
    let all = c.addrs.clone();

    // (a) isolate node 0 using only node 0's own config: both directions cut.
    c.p(0).set_chaos(ChaosConfig::isolate(all[0], &all)).unwrap();
    c.p(0).broadcast(status(1));
    for j in 1..4 {
        c.p(j).send(all[0], status(2));
    }
    std::thread::sleep(Duration::from_millis(300));
    for j in 1..4 {
        assert!(recv_heights(&c.rx(j).consensus, 1, Duration::from_millis(10)).is_empty());
    }
    assert!(recv_heights(&c.rx(0).consensus, 1, Duration::from_millis(10)).is_empty());
    // others still talk among themselves
    c.p(1).send(all[2], status(3));
    assert_eq!(recv_heights(&c.rx(2).consensus, 1, Duration::from_secs(2)), vec![(all[1], 3)]);
    // heal: instant
    let t0 = Instant::now();
    c.p(0).set_chaos_json("{}").unwrap();
    c.p(0).broadcast(status(4));
    for j in 1..4 {
        assert_eq!(recv_heights(&c.rx(j).consensus, 1, Duration::from_secs(2)), vec![(all[0], 4)]);
    }
    assert!(t0.elapsed() < Duration::from_millis(500), "heal took {:?}", t0.elapsed());
    let m = c.p(0).metrics();
    assert!(m.values().all(|p| p.chaos_dropped_out >= 1 && p.chaos_dropped_in >= 1), "{m:?}");

    // (b) 2|2 split applied everywhere via JSON.
    let json = format!(r#"{{"partitions": [["{}","{}"],["{}","{}"]]}}"#, all[0], all[1], all[2], all[3]);
    for i in 0..4 {
        c.p(i).set_chaos_json(&json).unwrap();
    }
    for i in 0..4 {
        c.p(i).broadcast(status(10 + i as u64));
    }
    for (i, partner) in [(0usize, 1usize), (1, 0), (2, 3), (3, 2)] {
        let got = recv_heights(&c.rx(i).consensus, 2, Duration::from_millis(500));
        assert_eq!(got, vec![(all[partner], 10 + partner as u64)], "node {i}");
    }
    for i in 0..4 {
        c.p(i).set_chaos(ChaosConfig::default()).unwrap();
    }
    for i in 0..4 {
        c.p(i).broadcast(status(20));
    }
    for i in 0..4 {
        assert_eq!(recv_heights(&c.rx(i).consensus, 3, Duration::from_secs(2)).len(), 3);
    }

    // (c) per-link drop 100% and blocked link; delay.
    c.p(1)
        .set_chaos(ChaosConfig {
            links: vec![
                LinkChaos { to: all[2], drop_prob: Some(1.0), ..Default::default() },
                LinkChaos { to: all[3], blocked: true, ..Default::default() },
            ],
            delay_ms_min: 150,
            delay_ms_max: 150,
            ..Default::default()
        })
        .unwrap();
    let t0 = Instant::now();
    c.p(1).broadcast(status(30));
    assert_eq!(recv_heights(&c.rx(0).consensus, 1, Duration::from_secs(2)), vec![(all[1], 30)]);
    assert!(t0.elapsed() >= Duration::from_millis(140), "delay applied: {:?}", t0.elapsed());
    std::thread::sleep(Duration::from_millis(300));
    assert!(c.rx(2).consensus.try_recv().is_err());
    assert!(c.rx(3).consensus.try_recv().is_err());
    // blocked link also cuts inbound at node 1
    c.p(3).send(all[1], status(31));
    std::thread::sleep(Duration::from_millis(200));
    assert!(c.rx(1).consensus.try_recv().is_err());
    c.p(1).set_chaos(ChaosConfig::default()).unwrap();
}

#[test]
fn chaos_duplicate_and_drop_statistics() {
    let c = Cluster::new(2, |_| {});
    c.wait_full_mesh();
    c.p(0)
        .set_chaos(ChaosConfig { drop_prob: 0.5, duplicate_prob: 0.0, seed: Some(3), ..Default::default() })
        .unwrap();
    for h in 0..1000 {
        c.p(0).send(c.addrs[1], status(h));
    }
    std::thread::sleep(Duration::from_millis(300));
    let n = c.rx(1).consensus.try_iter().count();
    assert!((350..650).contains(&n), "delivered {n}");
    c.p(0).set_chaos(ChaosConfig { duplicate_prob: 1.0, ..Default::default() }).unwrap();
    for h in 0..100 {
        c.p(0).send(c.addrs[1], status(h));
    }
    let got = recv_heights(&c.rx(1).consensus, 200, Duration::from_secs(3));
    assert_eq!(got.len(), 200);
}

#[test]
fn large_messages() {
    let c = Cluster::new(2, |_| {});
    c.wait_full_mesh();
    let data: Vec<u8> = (0..20 * 1024 * 1024).map(|i| (i * 7 % 256) as u8).collect();
    let snap = Snapshot { height: 9, state_root: Hash32([1; 32]), data };
    c.p(0).send(c.addrs[1], NetMsg::Consensus(ConsensusMsg::SnapshotResponse(Some(snap.clone()))));
    let (from, m) = c.rx(1).consensus.recv_timeout(Duration::from_secs(20)).unwrap();
    assert_eq!(from, c.addrs[0]);
    assert_eq!(m, NetMsg::Consensus(ConsensusMsg::SnapshotResponse(Some(snap))));
    // 20 MiB tx batch too
    let big = vec![utx(1, 20 * 1024 * 1024)];
    c.p(1).send(c.addrs[0], NetMsg::Txs(big.clone()));
    let (_, m) = c.rx(0).txs.recv_timeout(Duration::from_secs(20)).unwrap();
    assert_eq!(m, NetMsg::Txs(big));
    // > 32 MiB is refused locally; the link stays usable.
    c.p(0).send(c.addrs[1], NetMsg::Txs(vec![utx(2, 33 * 1024 * 1024)]));
    assert_eq!(c.p(0).oversize_dropped(), 1);
    c.p(0).send(c.addrs[1], status(1));
    assert_eq!(recv_heights(&c.rx(1).consensus, 1, Duration::from_secs(5)), vec![(c.addrs[0], 1)]);
    assert!(c.rx(1).txs.try_recv().is_err());
}

#[test]
fn tx_flood_does_not_block_consensus() {
    let c = Cluster::new(2, |cfg| cfg.inbound_tx_capacity = 8);
    c.wait_full_mesh();
    // Nobody drains the tx channel; flood it.
    for i in 0..200 {
        c.p(0).send(c.addrs[1], NetMsg::Txs(vec![utx(i, 1000)]));
    }
    c.p(0).send(c.addrs[1], status(77));
    assert_eq!(recv_heights(&c.rx(1).consensus, 1, Duration::from_secs(5)), vec![(c.addrs[0], 77)]);
    wait_until(Duration::from_secs(5), || c.p(1).metrics()[&c.addrs[0]].inbound_tx_dropped >= 150).unwrap();
    assert_eq!(c.rx(1).txs.len(), 8);
}

#[test]
fn outbound_tx_backpressure_drops_oldest_not_consensus() {
    // Tiny tx queue; peer's reads are slowed by a 1 s chaos delay? No: use a
    // burst far larger than the queue so the writer cannot keep up.
    let c = Cluster::new(2, |cfg| cfg.tx_queue_max_bytes = 64 * 1024);
    c.wait_full_mesh();
    for i in 0..2000 {
        c.p(0).send(c.addrs[1], NetMsg::Txs(vec![utx(i, 16 * 1024)]));
        if i % 100 == 0 {
            c.p(0).send(c.addrs[1], status(i));
        }
    }
    let got = recv_heights(&c.rx(1).consensus, 20, Duration::from_secs(10));
    assert_eq!(got.iter().map(|x| x.1).collect::<Vec<_>>(), (0..20).map(|i| i * 100).collect::<Vec<_>>());
    let m = c.p(0).metrics()[&c.addrs[1]].clone();
    assert!(m.tx_frames_dropped > 0, "{m:?}");
    // the newest tx batch always gets through
    wait_until(Duration::from_secs(5), || {
        c.rx(1).txs.try_iter().any(|(_, m)| matches!(&m, NetMsg::Txs(v) if v[0] == utx(1999, 16 * 1024)))
    })
    .unwrap();
}

/// Throughput on one localhost link. Run with
/// `cargo test -p net --release -- --ignored throughput --nocapture`.
#[test]
#[ignore]
fn throughput_single_link() {
    let c = Cluster::new(2, |_| {});
    c.wait_full_mesh();
    let batch: Vec<Tx> = (0..1000).map(|i| utx(i, 1000)).collect(); // ~1 MB
    let msg = NetMsg::Txs(batch);
    let rx = c.rx(1).txs.clone();
    let recv = std::thread::spawn(move || {
        let mut n = 0u64;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(4) {
            if let Ok((_, NetMsg::Txs(v))) = rx.recv_timeout(Duration::from_millis(100)) {
                n += v.iter().map(|t| t.encoded_len() as u64).sum::<u64>();
            }
        }
        n
    });
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(4) {
        if c.p(0).queued_bytes(&c.addrs[1]) > 32 << 20 {
            std::thread::sleep(Duration::from_micros(200));
            continue;
        }
        c.p(0).send(c.addrs[1], msg.clone());
    }
    let bytes = recv.join().unwrap();
    let mbps = bytes as f64 / 4.0 / 1e6;
    let m = c.p(0).metrics()[&c.addrs[1]].clone();
    println!("throughput: {mbps:.1} MB/s ({} frames dropped by backpressure)", m.tx_frames_dropped);
    assert!(mbps >= 50.0, "throughput {mbps:.1} MB/s < 50");
}
