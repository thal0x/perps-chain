//! Scenario tests over the deterministic simulator.

use consensus::sim::*;
use interfaces::*;

fn report(sim: &Sim) -> String {
    let mut s = String::new();
    for i in 0..sim.cfg.n {
        s += &format!(
            "node {i}: up={} h={} view={:?} {:?}\n",
            sim.is_up(i),
            sim.committed_height(i),
            sim.core(i).map(|c| interfaces::ConsensusCore::current_view(c)),
            sim.core(i).map(|c| c.stats().clone())
        );
    }
    s
}

/// After the network heals: commits must resume (+2 heights) within 10 s
/// and every listed honest node must catch up within 30 s.
fn assert_resumes(sim: &mut Sim, nodes: &[usize]) {
    let h0 = sim.max_honest_committed();
    let dt = sim.wait_for_height(h0 + 2, 10_000);
    assert!(dt.is_some(), "no progress within 10s after heal (h0={h0})\n{}", report(sim));
    let target = sim.max_honest_committed();
    let dt = sim.wait_all(nodes, target, 30_000);
    assert!(dt.is_some(), "nodes did not catch up to {target}\n{}", report(sim));
    sim.check_safety().unwrap();
}

#[test]
fn crashed_node_liveness_n4() {
    let mut sim = Sim::new(SimConfig::new(4, 2));
    sim.run_for(3_000);
    sim.crash(2);
    let h = sim.max_honest_committed();
    sim.run_for(20_000);
    assert!(sim.max_honest_committed() >= h + 20, "{}", report(&sim));
    sim.restart(2);
    assert_resumes(&mut sim, &[0, 1, 2, 3]);
}

#[test]
fn two_crashed_n7() {
    let mut sim = Sim::new(SimConfig::new(7, 3));
    sim.run_for(3_000);
    sim.crash(1);
    sim.crash(4);
    let h = sim.max_honest_committed();
    sim.run_for(30_000);
    assert!(sim.max_honest_committed() >= h + 20, "{}", report(&sim));
    sim.restart(1);
    sim.restart(4);
    assert_resumes(&mut sim, &[0, 1, 2, 3, 4, 5, 6]);
}

#[test]
fn split_2_2_halts_then_resumes() {
    let mut sim = Sim::new(SimConfig::new(4, 4));
    sim.run_for(3_000);
    sim.set_partition(&[vec![0, 1], vec![2, 3]]);
    sim.run_for(2_000);
    let h = sim.max_honest_committed();
    sim.run_for(60_000);
    assert!(sim.max_honest_committed() <= h + 1, "progress during a 2|2 split");
    sim.heal();
    assert_resumes(&mut sim, &[0, 1, 2, 3]);
}

#[test]
fn split_3_4_and_2_2_3_n7() {
    let mut sim = Sim::new(SimConfig::new(7, 5));
    sim.run_for(3_000);
    sim.set_partition(&[vec![0, 1, 2], vec![3, 4, 5, 6]]);
    sim.run_for(40_000);
    sim.heal();
    assert_resumes(&mut sim, &[0, 1, 2, 3, 4, 5, 6]);
    sim.set_partition(&[vec![0, 1], vec![2, 3], vec![4, 5, 6]]);
    sim.run_for(40_000);
    sim.heal();
    assert_resumes(&mut sim, &[0, 1, 2, 3, 4, 5, 6]);
}

#[test]
fn isolated_minority_keeps_majority_live() {
    let mut sim = Sim::new(SimConfig::new(7, 6));
    sim.run_for(2_000);
    sim.set_partition(&[vec![0, 1, 2, 3, 4]]); // 5 and 6 isolated
    let h = sim.max_honest_committed();
    sim.run_for(20_000);
    assert!(sim.max_honest_committed() >= h + 20, "{}", report(&sim));
    sim.heal();
    assert_resumes(&mut sim, &[0, 1, 2, 3, 4, 5, 6]);
}

#[test]
fn lossy_reordering_duplicating_network() {
    let mut cfg = SimConfig::new(4, 7);
    cfg.drop_ppm = 200_000;
    cfg.dup_ppm = 100_000;
    cfg.min_delay_ms = 0;
    cfg.max_delay_ms = 400;
    let mut sim = Sim::new(cfg);
    sim.run_for(60_000);
    sim.check_safety().unwrap();
    assert!(sim.max_honest_committed() >= 10, "{}", report(&sim));
    sim.drop_ppm = 0;
    sim.dup_ppm = 0;
    sim.max_delay_ms = 50;
    assert_resumes(&mut sim, &[0, 1, 2, 3]);
}

#[test]
fn equivocating_double_voting_leader_n4() {
    let mut sim = Sim::new(SimConfig::new(4, 8));
    sim.set_byzantine(1, Byzantine { equivocate: true, double_vote: true, stale_replay: false, lie_status: false });
    sim.run_for(40_000);
    sim.check_safety().unwrap();
    assert!(sim.max_honest_committed() >= 40, "{}", report(&sim));
    let eq: u64 = (0..4).filter_map(|i| sim.core(i)).map(|c| c.stats().equivocations).sum();
    assert!(eq > 0, "double votes not observed");
}

#[test]
fn two_byzantine_n7_all_behaviours() {
    let mut sim = Sim::new(SimConfig::new(7, 9));
    let b = Byzantine { equivocate: true, double_vote: true, stale_replay: true, lie_status: true };
    sim.set_byzantine(2, b.clone());
    sim.set_byzantine(5, b);
    sim.run_for(40_000);
    sim.check_safety().unwrap();
    assert!(sim.max_honest_committed() >= 40, "{}", report(&sim));
    sim.set_partition(&[vec![0, 1, 2, 3], vec![4, 5, 6]]);
    sim.run_for(20_000);
    sim.heal();
    assert_resumes(&mut sim, &[0, 1, 3, 4, 6]);
}

#[test]
fn restart_with_file_store_recovers() {
    let dir = std::env::temp_dir().join(format!("hlbft-cons-sim-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = SimConfig::new(4, 10);
    cfg.store = StoreKind::File { base: dir.clone(), retain_heights: 0 };
    let mut sim = Sim::new(cfg);
    sim.run_for(5_000);
    for round in 0..4 {
        let victim = round % 4;
        sim.crash(victim);
        sim.run_for(3_000 + 500 * round as u64);
        sim.restart(victim);
        sim.run_for(3_000);
    }
    assert!(sim.nodes.iter().all(|n| n.restarts >= 1));
    assert_resumes(&mut sim, &[0, 1, 2, 3]);
    // restarted nodes must have replayed their committed logs to the executor
    let top = sim.min_honest_committed();
    assert!(sim.wait_all(&[0, 1, 2, 3], top, 1_000).is_some());
    for n in &sim.nodes {
        assert!(n.exec.height + 5 >= top, "executor lagging after restart: {} vs {top}", n.exec.height);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn restart_all_nodes_mem_store() {
    let mut sim = Sim::new(SimConfig::new(4, 11));
    sim.run_for(5_000);
    let h = sim.max_honest_committed();
    for i in 0..4 {
        sim.crash(i);
    }
    sim.run_for(2_000);
    for i in 0..4 {
        sim.restart(i);
    }
    sim.run_for(15_000);
    assert!(sim.max_honest_committed() >= h + 10, "{}", report(&sim));
    sim.check_safety().unwrap();
}

#[test]
fn lagging_node_block_sync() {
    let mut cfg = SimConfig::new(4, 12);
    cfg.min_block_interval_ms = 50;
    let mut sim = Sim::new(cfg);
    sim.run_for(1_000);
    sim.crash(3);
    sim.run_for(150_000); // > one sync batch behind
    sim.restart(3);
    let target = sim.max_honest_committed();
    assert!(target > 300, "target {target}");
    assert!(sim.wait_all(&[3], target, 20_000).is_some(), "{}", report(&sim));
    assert!(sim.core(3).unwrap().stats().synced_blocks > 200);
    sim.check_safety().unwrap();
}

#[test]
fn snapshot_sync_when_peers_pruned() {
    let mut cfg = SimConfig::new(4, 13);
    cfg.store = StoreKind::Mem { retain_heights: 40 };
    cfg.snapshot_every = 20;
    let mut sim = Sim::new(cfg);
    sim.run_for(1_000);
    sim.crash(3);
    sim.run_for(40_000);
    sim.restart(3);
    let target = sim.max_honest_committed();
    assert!(sim.wait_all(&[3], target, 30_000).is_some(), "{}", report(&sim));
    let st = sim.core(3).unwrap().stats().clone();
    assert!(st.snapshots_installed >= 1, "{st:?}");
    sim.run_for(5_000);
    // executor of node 3 continued from the snapshot and agrees on roots
    let n3 = &sim.nodes[3];
    let h = n3.exec.height;
    assert!(h + 5 >= sim.committed_height(3));
    assert_eq!(sim.global_roots.get(&h), n3.exec.roots.get(&h));
    sim.check_safety().unwrap();
}

#[test]
fn exec_roots_certified_and_consistent() {
    let mut sim = Sim::new(SimConfig::new(4, 14));
    sim.run_for(15_000);
    sim.check_safety().unwrap();
    // every committed header's exec claim equals the honest root at that height
    let top = sim.committed_height(0);
    let mut checked = 0;
    for h in 1..=top {
        let cb = sim.nodes[0].store.committed_at(h).unwrap();
        let eh = cb.block.header.exec_height;
        if eh == 0 {
            continue;
        }
        assert_eq!(sim.global_roots.get(&eh), Some(&cb.block.header.exec_root), "height {h} claims root for {eh}");
        checked += 1;
    }
    assert!(checked > 20);
    // exec lag is small
    let last = sim.nodes[0].store.committed_at(top).unwrap();
    assert!(top - last.block.header.exec_height <= 4);
    assert_eq!(sim.total_divergences(), 0);
}

#[test]
fn diverging_executor_is_detected() {
    let mut sim = Sim::new(SimConfig::new(4, 15));
    sim.set_diverging_executor(2);
    sim.run_for(15_000);
    // honest nodes keep committing; divergence is reported loudly
    assert!(sim.max_honest_committed() >= 30);
    let d2 = sim.core(2).unwrap().stats().divergences;
    let others: u64 = [0, 1, 3].iter().map(|i| sim.core(*i).unwrap().stats().divergences).sum();
    assert!(d2 > 0, "diverging node did not detect divergence");
    assert!(others > 0, "honest nodes did not detect the diverging proposer");
    sim.check_safety().unwrap();
}

#[test]
fn replay_override_is_followed() {
    let mut log = std::collections::BTreeMap::new();
    for h in 1..=40u64 {
        // timestamps far ahead of the wall clock: replay mode skips the drift check
        let ts = SIM_GENESIS_TIME + 1_000_000 + h * 6_000;
        let txs = (0..h % 5)
            .map(|k| {
                Tx::User(UserTx {
                    action_bytes: vec![h as u8, k as u8, 9],
                    nonce: h * 10 + k,
                    signature: Sig65::default(),
                    vault_address: None,
                    expires_after: None,
                })
            })
            .collect::<Vec<_>>();
        log.insert(h, (ts, txs));
    }
    let log = std::sync::Arc::new(log);
    let mut cfg = SimConfig::new(4, 16);
    cfg.min_block_interval_ms = 50;
    let mut sim = Sim::new(cfg);
    sim.replay = Some(log.clone());
    for i in 0..4 {
        sim.crash(i);
        sim.restart(i);
    }
    // the last two logged blocks need successors (2-chain) to commit; past
    // the end of the log the override returns None and proposals fall back
    // to the wall clock, which is behind the logged timestamps -> halt.
    sim.wait_all(&[0, 1, 2, 3], 38, 30_000).expect("replay did not reach height 38");
    sim.run_for(10_000);
    assert!(sim.max_honest_committed() <= 40);
    for h in 1..=38u64 {
        let cb = sim.nodes[1].store.committed_at(h).unwrap();
        let (ts, txs) = &log[&h];
        assert_eq!(cb.block.header.timestamp_ms, *ts);
        assert_eq!(&cb.block.payload, txs);
    }
    sim.check_safety().unwrap();
}

#[test]
fn unequal_stakes() {
    // one validator holds > 1/3: it is required for every quorum
    let mut cfg = SimConfig::new(4, 17);
    cfg.stakes = Some(vec![4, 1, 1, 1]);
    let mut sim = Sim::new(cfg);
    sim.run_for(5_000);
    let h = sim.max_honest_committed();
    assert!(h >= 15);
    sim.crash(1);
    sim.crash(2); // remaining stake 5/7 > 2/3: still live
    sim.run_for(20_000);
    assert!(sim.max_honest_committed() > h + 10, "{}", report(&sim));
    sim.restart(1);
    sim.restart(2);
    sim.crash(0); // 3/7 left: must halt
    sim.run_for(3_000);
    let h2 = sim.max_honest_committed();
    sim.run_for(20_000);
    assert!(sim.max_honest_committed() <= h2 + 1);
    sim.restart(0);
    assert_resumes(&mut sim, &[0, 1, 2, 3]);
}

#[test]
fn large_blocks_20k_txs() {
    let mut cfg = SimConfig::new(4, 18);
    cfg.txs_per_block = 20_000;
    cfg.tx_bytes = 100;
    let mut sim = Sim::new(cfg);
    let t = std::time::Instant::now();
    sim.run_for(4_000);
    let el = t.elapsed();
    let h = sim.max_honest_committed();
    println!("20k-tx blocks: {h} heights in 4s virtual, {el:?} wall (4 nodes, incl. sim cloning)");
    assert!(h >= 12);
    sim.check_safety().unwrap();
}


#[test]
fn every_snapshot_height_is_claimed_by_a_header() {
    // fast cadence: execution lag varies, plain claims skip heights; claims
    // must never skip a snapshot height (snapshot sync certifies by exact claim)
    let mut cfg = SimConfig::new(4, 21);
    cfg.min_block_interval_ms = 50;
    cfg.snapshot_every = 20;
    let mut sim = Sim::new(cfg);
    sim.run_for(30_000);
    let top = sim.committed_height(0);
    let claims: Vec<u64> = (1..=top).map(|h| sim.nodes[0].store.committed_at(h).unwrap().block.header.exec_height).collect();
    let claimed: std::collections::BTreeSet<u64> = claims.iter().copied().collect();
    let missing: Vec<u64> = (1..top - 10).filter(|h| h % 20 == 0 && !claimed.contains(h)).collect();
    assert!(top > 300 && missing.is_empty(), "top {top}, unclaimed snapshot heights: {missing:?}");
    // and claims keep up with execution (lag stays small)
    assert!(top - claims[claims.len() - 1] <= 6, "claim lag {}", top - claims[claims.len() - 1]);
}
