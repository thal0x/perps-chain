//! Certificate verification and state-sync attack tests (Core driven
//! directly with a hand-written env).

use consensus::crypto::{self, CertCtx, VoteKind};
use consensus::sim::*;
use consensus::*;
use interfaces::*;

struct TestEnv {
    now: u64,
    me: Address,
    store: MemStore,
    roots: std::collections::BTreeMap<Height, Hash32>,
}

impl ConsensusEnv for TestEnv {
    fn now_ms(&self) -> u64 {
        self.now
    }
    fn payload(&mut self, _: usize, _: usize) -> Vec<Tx> {
        vec![]
    }
    fn exec_state(&self) -> (Height, Hash32) {
        self.roots.iter().next_back().map(|(h, r)| (*h, *r)).unwrap_or((0, Hash32::ZERO))
    }
    fn root_at(&self, h: Height) -> Option<Hash32> {
        self.roots.get(&h).copied()
    }
    fn snapshot(&self) -> Option<Snapshot> {
        None
    }
    fn sign(&self, d: &[u8; 32]) -> Sig65 {
        FakeKeys::sign(&self.me, d)
    }
    fn verify(&self, d: &[u8; 32], s: &Sig65, a: &Address) -> bool {
        FakeKeys::verify(d, s, a)
    }
    fn store(&mut self) -> &mut dyn BlockStore {
        &mut self.store
    }
}

fn vset(n: usize) -> ValidatorSet {
    ValidatorSet { validators: (0..n).map(|i| ValidatorInfo { address: sim_address(i), stake: 1, p2p_addr: String::new() }).collect() }
}

fn qc(data: VoteData, signers: &[usize], kind: VoteKind) -> QuorumCert {
    let d = crypto::vote_digest(&data, kind);
    QuorumCert { signatures: signers.iter().map(|i| (sim_address(*i), FakeKeys::sign(&sim_address(*i), &d))).collect(), data }
}

fn ctx(vs: &ValidatorSet) -> CertCtx<'_> {
    CertCtx { chain_id: SIM_CHAIN_ID, genesis_id: genesis_block(SIM_CHAIN_ID, SIM_GENESIS_TIME).id(), validators: vs }
}

const V: &dyn Fn(&[u8; 32], &Sig65, &Address) -> bool = &FakeKeys::verify;

#[test]
fn qc_verification_rules() {
    let vs = vset(4);
    let c = ctx(&vs);
    let data = VoteData { chain_id: SIM_CHAIN_ID, block_id: Hash32([5; 32]), view: 9, height: 4 };
    assert_eq!(c.verify_qc(&qc(data.clone(), &[0, 1, 2], VoteKind::Direct), V), Ok(VoteKind::Direct));
    assert_eq!(c.verify_qc(&qc(data.clone(), &[3, 1, 2], VoteKind::AfterTimeout), V), Ok(VoteKind::AfterTimeout));
    assert!(c.verify_qc(&qc(data.clone(), &[0, 1], VoteKind::Direct), V).is_err(), "2 of 4 is not a quorum");
    assert!(c.verify_qc(&qc(data.clone(), &[0, 1, 1], VoteKind::Direct), V).is_err(), "duplicate signer");
    // non-validator signer
    let mut q = qc(data.clone(), &[0, 1, 2], VoteKind::Direct);
    q.signatures[2] = (sim_address(9), FakeKeys::sign(&sim_address(9), &crypto::vote_digest(&data, VoteKind::Direct)));
    assert!(c.verify_qc(&q, V).is_err());
    // mixed kinds
    let mut q = qc(data.clone(), &[0, 1, 2], VoteKind::Direct);
    q.signatures[1].1 = FakeKeys::sign(&sim_address(1), &crypto::vote_digest(&data, VoteKind::AfterTimeout));
    assert!(c.verify_qc(&q, V).is_err());
    // tampered data
    let mut q = qc(data.clone(), &[0, 1, 2], VoteKind::Direct);
    q.data.height = 5;
    assert!(c.verify_qc(&q, V).is_err());
    // wrong chain
    let mut d2 = data.clone();
    d2.chain_id = 1;
    assert!(c.verify_qc(&qc(d2, &[0, 1, 2], VoteKind::Direct), V).is_err());
    // genesis qc
    assert!(c.verify_qc(&genesis_qc(SIM_CHAIN_ID, genesis_block(SIM_CHAIN_ID, SIM_GENESIS_TIME).id()), V).is_ok());
    // unsigned non-genesis
    assert!(c.verify_qc(&QuorumCert { data, signatures: vec![] }, V).is_err());
}

#[test]
fn commit_proof_requires_direct_child() {
    let vs = vset(4);
    let c = ctx(&vs);
    let b = Hash32([1; 32]);
    let q0 = qc(VoteData { chain_id: SIM_CHAIN_ID, block_id: b, view: 10, height: 7 }, &[0, 1, 2], VoteKind::AfterTimeout);
    let child = VoteData { chain_id: SIM_CHAIN_ID, block_id: Hash32([2; 32]), view: 11, height: 8 };
    let direct = qc(child.clone(), &[1, 2, 3], VoteKind::Direct);
    let via_tc = qc(child.clone(), &[1, 2, 3], VoteKind::AfterTimeout);
    assert!(c.verify_direct_commit_proof(&b, 7, &[q0.clone(), direct.clone()], V).is_ok());
    assert!(c.verify_direct_commit_proof(&b, 7, &[q0.clone(), via_tc], V).is_err(), "child via TC proves nothing");
    let mut gap = child.clone();
    gap.view = 12;
    assert!(c.verify_direct_commit_proof(&b, 7, &[q0.clone(), qc(gap, &[1, 2, 3], VoteKind::Direct)], V).is_err());
    assert!(c.verify_direct_commit_proof(&Hash32([9; 32]), 7, &[q0, direct], V).is_err());
}

#[test]
fn tc_verification_rules() {
    let vs = vset(4);
    let c = ctx(&vs);
    let hq = qc(VoteData { chain_id: SIM_CHAIN_ID, block_id: Hash32([3; 32]), view: 4, height: 3 }, &[0, 1, 2], VoteKind::Direct);
    let mk = |who: &[(usize, u64)], high: QuorumCert| TimeoutCert {
        view: 6,
        timeouts: who
            .iter()
            .map(|(i, hv)| {
                let d = crypto::timeout_digest(&TimeoutData { chain_id: SIM_CHAIN_ID, view: 6, high_qc_view: *hv });
                (sim_address(*i), *hv, FakeKeys::sign(&sim_address(*i), &d))
            })
            .collect(),
        high_qc: high,
    };
    assert!(c.verify_tc(&mk(&[(0, 4), (1, 2), (3, 4)], hq.clone()), V).is_ok());
    assert!(c.verify_tc(&mk(&[(0, 4), (1, 2)], hq.clone()), V).is_err(), "no quorum");
    assert!(c.verify_tc(&mk(&[(0, 4), (1, 5), (3, 4)], hq.clone()), V).is_err(), "high_qc not the max");
    let mut t = mk(&[(0, 4), (1, 2), (3, 4)], hq);
    t.timeouts[1].1 = 3; // signed 2, claims 3
    assert!(c.verify_tc(&t, V).is_err());
}

/// Produce a real committed chain with the simulator.
fn real_chain() -> (Vec<CommittedBlock>, Sim) {
    let mut sim = Sim::new(SimConfig::new(4, 99));
    sim.run_for(12_000);
    let top = sim.committed_height(0);
    let blocks: Vec<CommittedBlock> = (1..=top).map(|h| sim.nodes[0].store.committed_at(h).unwrap()).collect();
    (blocks, sim)
}

fn fresh_core(me: usize) -> (Core, TestEnv) {
    let mut env = TestEnv { now: SIM_GENESIS_TIME + 100_000, me: sim_address(me), store: MemStore::new(), roots: Default::default() };
    let cfg = CoreConfig::new(SIM_CHAIN_ID, SIM_GENESIS_TIME, vset(4), sim_address(me));
    let core = Core::new(cfg, &mut env);
    (core, env)
}

fn commits(out: &[ConsensusOutput]) -> Vec<Height> {
    out.iter()
        .filter_map(|o| match o {
            ConsensusOutput::Commit(c) => Some(c.block.header.height),
            _ => None,
        })
        .collect()
}

fn sync_once(core: &mut Core, env: &mut TestEnv, blocks: Vec<CommittedBlock>, pch: Height) -> Vec<ConsensusOutput> {
    let from = sim_address(0);
    // make the core want to sync from node 0
    let mut out = core.handle(ConsensusInput::Message { from, msg: ConsensusMsg::Status { committed_height: pch, high_qc_view: 0 } }, env);
    out.extend(core.handle(ConsensusInput::Message { from, msg: ConsensusMsg::SyncResponse { blocks, peer_committed_height: pch } }, env));
    out
}

#[test]
fn sync_accepts_valid_chain() {
    let (chain, _) = real_chain();
    assert!(chain.len() > 30);
    let (mut core, mut env) = fresh_core(3);
    let out = sync_once(&mut core, &mut env, chain[..20].to_vec(), chain.len() as u64);
    let c = commits(&out);
    assert!(!c.is_empty());
    assert_eq!(c, (1..=c.len() as u64).collect::<Vec<_>>(), "strict height order");
    for h in &c {
        assert_eq!(env.store.committed_at(*h).unwrap().block, chain[*h as usize - 1].block);
    }
}

#[test]
fn sync_rejects_tampered_payload_and_forged_qcs() {
    let (chain, _) = real_chain();
    // tampered payload in block 5: nothing at/after 5 may be committed
    let mut bad = chain[..20].to_vec();
    bad[4].block.payload.push(Tx::User(UserTx {
        action_bytes: vec![1, 2, 3],
        nonce: 1,
        signature: Sig65::default(),
        vault_address: None,
        expires_after: None,
    }));
    let (mut core, mut env) = fresh_core(3);
    let out = sync_once(&mut core, &mut env, bad, 100);
    assert!(commits(&out).iter().all(|h| *h < 5), "{:?}", commits(&out));

    // header tampered (exec root changed) -> id changes -> proof mismatch
    let mut bad = chain[..20].to_vec();
    bad[0].block.header.exec_root = Hash32([0xee; 32]);
    let (mut core, mut env) = fresh_core(3);
    assert!(commits(&sync_once(&mut core, &mut env, bad, 100)).is_empty());

    // forged signatures in every proof
    let mut bad = chain[..20].to_vec();
    for cb in &mut bad {
        for q in &mut cb.commit_proof {
            for s in &mut q.signatures {
                s.1.r[0] ^= 1;
            }
        }
    }
    let (mut core, mut env) = fresh_core(3);
    assert!(commits(&sync_once(&mut core, &mut env, bad, 100)).is_empty());

    // a run whose top lacks a direct proof is not accepted beyond the last direct one
    let mut bad = chain[..20].to_vec();
    for cb in &mut bad[10..] {
        cb.commit_proof.truncate(1);
    }
    let (mut core, mut env) = fresh_core(3);
    assert!(commits(&sync_once(&mut core, &mut env, bad, 100)).iter().all(|h| *h <= 10));
}

#[test]
fn snapshot_requires_certified_root() {
    let (chain, _) = real_chain();
    let from = sim_address(0);
    // find a height s whose root is certified by a later header
    let cert = chain.iter().find(|cb| cb.block.header.exec_height >= 10).unwrap().block.header.clone();
    let s = cert.exec_height;
    for (root, expect) in [(Hash32([0xab; 32]), false), (cert.exec_root, true)] {
        let (mut core, mut env) = fresh_core(3);
        let pch = chain.len() as u64;
        let mut out = core.handle(ConsensusInput::Message { from, msg: ConsensusMsg::Status { committed_height: pch, high_qc_view: 0 } }, &mut env);
        // peer has pruned everything -> empty response -> snapshot request
        out.extend(core.handle(
            ConsensusInput::Message { from, msg: ConsensusMsg::SyncResponse { blocks: vec![], peer_committed_height: pch } },
            &mut env,
        ));
        assert!(out.iter().any(|o| matches!(o, ConsensusOutput::Send { msg: ConsensusMsg::SnapshotRequest, .. })));
        let snap = Snapshot { height: s, state_root: root, data: vec![] };
        let out = core.handle(ConsensusInput::Message { from, msg: ConsensusMsg::SnapshotResponse(Some(snap)) }, &mut env);
        assert!(out.iter().any(|o| matches!(o, ConsensusOutput::Send { msg: ConsensusMsg::SyncRequest { from: f, .. }, .. } if *f == s + 1)));
        let blocks = chain[s as usize..].to_vec();
        let out = core.handle(
            ConsensusInput::Message { from, msg: ConsensusMsg::SyncResponse { blocks, peer_committed_height: pch } },
            &mut env,
        );
        let installed = out.iter().any(|o| matches!(o, ConsensusOutput::InstallSnapshot(x) if x.height == s));
        assert_eq!(installed, expect, "root {root}");
        if expect {
            let c = commits(&out);
            assert_eq!(c.first(), Some(&(s + 1)));
            // InstallSnapshot precedes the first commit
            let pi = out.iter().position(|o| matches!(o, ConsensusOutput::InstallSnapshot(_))).unwrap();
            let pc = out.iter().position(|o| matches!(o, ConsensusOutput::Commit(_))).unwrap();
            assert!(pi < pc);
        } else {
            assert!(commits(&out).is_empty());
        }
    }
}

#[test]
fn committed_log_is_externally_verifiable() {
    // what the chaos checker does with committed.log: verify every proof
    let (chain, sim) = real_chain();
    let vs = sim.validators.clone();
    let c = ctx(&vs);
    let g = genesis_block(SIM_CHAIN_ID, SIM_GENESIS_TIME).id();
    assert_eq!(c.verify_committed_run(&chain, Some((0, g)), V), chain.len());
}
