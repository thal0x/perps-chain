//! Threaded Driver over an in-memory network with real secp256k1 keys and
//! the wall clock, and a FileStore restart.

use consensus::crypto::secp;
use consensus::*;
use crossbeam_channel::{unbounded, Receiver, Sender};
use interfaces::*;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

#[derive(Clone, Default)]
struct Hub {
    inboxes: Arc<Mutex<BTreeMap<Address, Sender<(Address, ConsensusMsg)>>>>,
}

struct MemNet {
    me: Address,
    hub: Hub,
}

impl Network for MemNet {
    fn local(&self) -> Address {
        self.me
    }
    fn send(&self, to: Address, msg: NetMsg) {
        if let NetMsg::Consensus(m) = msg {
            if let Some(tx) = self.hub.inboxes.lock().unwrap().get(&to) {
                let _ = tx.send((self.me, m));
            }
        }
    }
    fn broadcast(&self, msg: NetMsg) {
        if let NetMsg::Consensus(m) = msg {
            for (a, tx) in self.hub.inboxes.lock().unwrap().iter() {
                if *a != self.me {
                    let _ = tx.send((self.me, m.clone()));
                }
            }
        }
    }
    fn peers(&self) -> Vec<Address> {
        self.hub.inboxes.lock().unwrap().keys().copied().filter(|a| *a != self.me).collect()
    }
}

#[derive(Default)]
struct ExecState {
    height: Height,
    root: Hash32,
    roots: BTreeMap<Height, Hash32>,
}

struct NodeEnv {
    sk: secp256k1::SecretKey,
    me: Address,
    store: Box<dyn BlockStore>,
    exec: Arc<Mutex<ExecState>>,
    n: u64,
}

impl ConsensusEnv for NodeEnv {
    fn now_ms(&self) -> u64 {
        now_ms()
    }
    fn payload(&mut self, _: usize, _: usize) -> Vec<Tx> {
        self.n += 1;
        vec![Tx::User(UserTx { action_bytes: self.me.0.to_vec(), nonce: self.n, signature: Sig65::default(), vault_address: None, expires_after: None })]
    }
    fn exec_state(&self) -> (Height, Hash32) {
        let e = self.exec.lock().unwrap();
        (e.height, e.root)
    }
    fn root_at(&self, h: Height) -> Option<Hash32> {
        if h == 0 {
            return Some(Hash32::ZERO);
        }
        self.exec.lock().unwrap().roots.get(&h).copied()
    }
    fn snapshot(&self) -> Option<Snapshot> {
        self.store.latest_snapshot()
    }
    fn sign(&self, d: &[u8; 32]) -> Sig65 {
        secp::sign(&self.sk, d)
    }
    fn verify(&self, d: &[u8; 32], s: &Sig65, a: &Address) -> bool {
        secp::verify(d, s, a)
    }
    fn store(&mut self) -> &mut dyn BlockStore {
        self.store.as_mut()
    }
}

struct Node {
    handle: DriverHandle,
    commits: Arc<Mutex<Vec<(Height, BlockId)>>>,
    exec: Arc<Mutex<ExecState>>,
}

fn keys(n: usize) -> Vec<(secp256k1::SecretKey, Address)> {
    (0..n)
        .map(|i| {
            let sk = secp256k1::SecretKey::from_slice(&[i as u8 + 1; 32]).unwrap();
            (sk, secp::address_of_secret(&sk))
        })
        .collect()
}

fn start_node(
    i: usize,
    ks: &[(secp256k1::SecretKey, Address)],
    vs: &ValidatorSet,
    genesis_ms: u64,
    hub: &Hub,
    store: Box<dyn BlockStore>,
    inbox: Receiver<(Address, ConsensusMsg)>,
) -> Node {
    let (sk, me) = ks[i];
    let mut cfg = CoreConfig::new(42, genesis_ms, vs.clone(), me);
    cfg.min_block_interval_ms = 100;
    cfg.base_timeout_ms = 500;
    let exec = Arc::new(Mutex::new(ExecState::default()));
    let env = NodeEnv { sk, me, store, exec: exec.clone(), n: 0 };
    let commits = Arc::new(Mutex::new(vec![]));
    let (etx, erx) = unbounded::<ConsensusOutput>();
    let net = Arc::new(MemNet { me, hub: hub.clone() });
    let handle = Driver::spawn(cfg, env, net, inbox, Box::new(move |o| {
        let _ = etx.send(o);
    }));
    // executor thread: root = blake3(prev || id)
    let feedback = handle.sender();
    let (c2, e2) = (commits.clone(), exec.clone());
    std::thread::spawn(move || {
        for o in erx {
            if let ConsensusOutput::Commit(cb) = o {
                let id = cb.block.id();
                c2.lock().unwrap().push((cb.block.header.height, id));
                let mut e = e2.lock().unwrap();
                let mut h = blake3::Hasher::new();
                h.update(&e.root.0);
                h.update(&id.0);
                e.root = Hash32(*h.finalize().as_bytes());
                e.height = cb.block.header.height;
                let (hh, rr) = (e.height, e.root);
                e.roots.insert(hh, rr);
                drop(e);
                let _ = feedback.send(DriverEvent::Executed { height: hh, root: rr });
            }
        }
    });
    Node { handle, commits, exec }
}

fn wait_until(deadline: Duration, mut f: impl FnMut() -> bool) -> bool {
    let t = Instant::now();
    while t.elapsed() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

#[test]
fn driver_four_nodes_secp256k1_with_restart() {
    let ks = keys(4);
    let vs = ValidatorSet { validators: ks.iter().map(|(_, a)| ValidatorInfo { address: *a, stake: 1, p2p_addr: String::new() }).collect() };
    let genesis_ms = now_ms();
    let hub = Hub::default();
    let base = std::env::temp_dir().join(format!("hlbft-cons-driver-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let mut nodes = vec![];
    for i in 0..4 {
        let (tx, rx) = unbounded();
        hub.inboxes.lock().unwrap().insert(ks[i].1, tx);
        let store = Box::new(FileStore::open(base.join(format!("n{i}"))).unwrap());
        nodes.push(Some(start_node(i, &ks, &vs, genesis_ms, &hub, store, rx)));
    }
    assert!(
        wait_until(Duration::from_secs(20), || nodes.iter().all(|n| n.as_ref().unwrap().handle.status().committed_height.load(std::sync::atomic::Ordering::Relaxed) >= 10)),
        "drivers did not commit 10 blocks"
    );
    // restart node 2 from its FileStore
    let n2 = nodes[2].take().unwrap();
    let before = n2.commits.lock().unwrap().len();
    n2.handle.shutdown();
    hub.inboxes.lock().unwrap().remove(&ks[2].1);
    std::thread::sleep(Duration::from_millis(700));
    let (tx, rx) = unbounded();
    hub.inboxes.lock().unwrap().insert(ks[2].1, tx);
    let store = Box::new(FileStore::open(base.join("n2")).unwrap());
    nodes[2] = Some(start_node(2, &ks, &vs, genesis_ms, &hub, store, rx));
    let target = nodes[0].as_ref().unwrap().handle.status().committed_height.load(std::sync::atomic::Ordering::Relaxed) + 5;
    assert!(
        wait_until(Duration::from_secs(20), || nodes.iter().all(|n| n.as_ref().unwrap().exec.lock().unwrap().height >= target)),
        "nodes did not reach {target} after restart"
    );
    // restarted node replayed its committed log from height 1 into its fresh executor
    let c2 = nodes[2].as_ref().unwrap().commits.lock().unwrap().clone();
    assert!(before > 0 && c2.first().map(|x| x.0) == Some(1));
    // all nodes agree on block ids and roots for common heights
    let c0: BTreeMap<Height, BlockId> = nodes[0].as_ref().unwrap().commits.lock().unwrap().iter().copied().collect();
    for n in nodes.iter().flatten() {
        for (h, id) in n.commits.lock().unwrap().iter() {
            if let Some(x) = c0.get(h) {
                assert_eq!(x, id, "height {h}");
            }
        }
        assert_eq!(n.handle.status().divergences.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
    let r0 = nodes[0].as_ref().unwrap().exec.lock().unwrap().roots.get(&target).copied();
    for n in nodes.iter().flatten() {
        assert_eq!(n.exec.lock().unwrap().roots.get(&target).copied(), r0);
    }
    for n in nodes.into_iter().flatten() {
        n.handle.shutdown();
    }
    let _ = std::fs::remove_dir_all(&base);
}
