//! Randomised fault schedules (seeded) for n in {4, 7} with up to f faulty
//! validators (Byzantine, crashed, isolated or lossy), plus whole-network
//! splits. After every heal commits must resume within 10 s of virtual time;
//! safety is checked continuously.
//!
//! Default run: a handful of seeds per configuration.
//! Long sweep:  `HLBFT_SIM_SEEDS=500 cargo test -p consensus --release --test sim_prop -- --ignored --nocapture`

use consensus::sim::*;
use proptest::prelude::*;
use rand::{seq::SliceRandom, Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

#[derive(Debug)]
struct Outcome {
    heals: u32,
    max_resume_ms: u64,
    height: u64,
}

fn run_schedule(n: usize, seed: u64, duration_ms: u64) -> Result<Outcome, String> {
    let f = (n - 1) / 3;
    let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0x5eed);
    let mut cfg = SimConfig::new(n, seed);
    cfg.txs_per_block = rng.gen_range(0..8);
    cfg.min_block_interval_ms = [50, 100, 200][rng.gen_range(0..3)];
    if rng.gen_range(0..3) == 0 {
        cfg.store = StoreKind::Mem { retain_heights: 60 };
        cfg.snapshot_every = 20;
    }
    let mut sim = Sim::new(cfg);
    let mut order: Vec<usize> = (0..n).collect();
    order.shuffle(&mut rng);
    let n_byz = rng.gen_range(0..=f);
    let byz: Vec<usize> = order[..n_byz].to_vec();
    for &b in &byz {
        sim.set_byzantine(
            b,
            Byzantine {
                equivocate: rng.gen_bool(0.7),
                double_vote: rng.gen_bool(0.7),
                stale_replay: rng.gen_bool(0.7),
                lie_status: rng.gen_bool(0.3),
            },
        );
    }
    let honest: Vec<usize> = (0..n).filter(|i| !byz.contains(i)).collect();
    let budget = f - n_byz; // additional faulty (crashed/isolated) honest nodes allowed
    let mut out = Outcome { heals: 0, max_resume_ms: 0, height: 0 };
    sim.run_for(2_000);
    while sim.now < SIM_GENESIS_TIME + duration_ms {
        // ---- fault episode
        let mut crashed = vec![];
        let kind = rng.gen_range(0..5);
        let mut victims = honest.clone();
        victims.shuffle(&mut rng);
        let k = if budget == 0 { 0 } else { rng.gen_range(1..=budget) };
        let victims: Vec<usize> = victims[..k].to_vec();
        match kind {
            0 => {
                for &v in &victims {
                    sim.crash(v);
                    crashed.push(v);
                }
            }
            1 => {
                // isolate victims, the rest stays connected
                let rest: Vec<usize> = (0..n).filter(|i| !victims.contains(i)).collect();
                let mut groups = vec![rest];
                for &v in &victims {
                    groups.push(vec![v]);
                }
                sim.set_partition(&groups);
            }
            2 => {
                // whole-network split: no side has a quorum
                let mut all: Vec<usize> = (0..n).collect();
                all.shuffle(&mut rng);
                let q = n - f; // quorum size with equal stakes
                let cut = rng.gen_range((n - q + 1)..q);
                sim.set_partition(&[all[..cut].to_vec(), all[cut..].to_vec()]);
            }
            3 => {
                sim.drop_ppm = rng.gen_range(0..=300_000);
                sim.dup_ppm = rng.gen_range(0..=100_000);
                sim.min_delay_ms = 0;
                sim.max_delay_ms = rng.gen_range(50..=2_000);
            }
            _ => {
                // crash + restart quickly (restart with persisted store)
                for &v in &victims {
                    sim.crash(v);
                }
                sim.run_for(rng.gen_range(10..2_000));
                for &v in &victims {
                    sim.restart(v);
                }
            }
        }
        sim.run_for(rng.gen_range(5_000..30_000));
        sim.check_safety()?;
        // ---- heal
        for v in crashed {
            sim.restart(v);
        }
        sim.heal();
        sim.drop_ppm = 0;
        sim.dup_ppm = 0;
        sim.min_delay_ms = 5;
        sim.max_delay_ms = 50;
        out.heals += 1;
        let h0 = sim.max_honest_committed();
        let start = sim.now;
        match sim.wait_for_height(h0 + 2, 10_000) {
            Some(_) => {
                out.max_resume_ms = out.max_resume_ms.max(sim.now - start);
            }
            None => {
                let mut s = format!("seed {seed} n {n}: no progress within 10s after heal #{} (h0={h0}, kind={kind})\n", out.heals);
                for i in 0..n {
                    s += &format!(
                        "  node {i} byz={} up={} h={} view={:?}\n",
                        !sim.is_honest(i),
                        sim.is_up(i),
                        sim.committed_height(i),
                        sim.core(i).map(interfaces::ConsensusCore::current_view)
                    );
                }
                return Err(s);
            }
        }
        sim.run_for(rng.gen_range(1_000..5_000));
        sim.check_safety()?;
    }
    // everybody honest catches up eventually
    let target = sim.max_honest_committed();
    if sim.wait_all(&honest, target, 60_000).is_none() {
        let mut s = format!("seed {seed} n {n}: honest nodes did not converge to {target} (now={})\n", sim.now - SIM_GENESIS_TIME);
        for i in 0..n {
            s += &format!("  node {i} byz={:?} up={} h={} exec={} view={:?} {:?}\n", sim.nodes[i].byz, sim.is_up(i), sim.committed_height(i), sim.nodes[i].exec.height,
                sim.core(i).map(interfaces::ConsensusCore::current_view), sim.core(i).map(|c| c.stats().clone()));
        }
        return Err(s);
    }
    sim.check_safety()?;
    if sim.total_safety_violations() > 0 {
        return Err("core reported safety violation".into());
    }
    out.height = target;
    Ok(out)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 16, failure_persistence: None, .. ProptestConfig::default() })]
    #[test]
    fn random_faults_n4(seed in any::<u64>()) {
        let o = run_schedule(4, seed, 120_000).map_err(TestCaseError::fail)?;
        prop_assert!(o.height > 10);
    }

    #[test]
    fn random_faults_n7(seed in any::<u64>()) {
        let o = run_schedule(7, seed, 120_000).map_err(TestCaseError::fail)?;
        prop_assert!(o.height > 10);
    }
}

#[test]
fn fixed_seeds_regression() {
    for seed in 0..8u64 {
        for n in [4, 7] {
            let o = run_schedule(n, seed, 90_000).unwrap_or_else(|e| panic!("{e}"));
            assert!(o.max_resume_ms <= 10_000);
        }
    }
}

/// Regression: a restarted node far behind pruned peers could never certify
/// a snapshot because header exec claims skipped heights (found in the
/// main merge gate).
#[test]
fn regression_snapshot_claim_gap() {
    let o = run_schedule(4, 7182031906024167429, 120_000).unwrap_or_else(|e| panic!("{e}"));
    assert!(o.height > 10);
}

#[test]
#[ignore]
fn long_seed_sweep() {
    let seeds: u64 = std::env::var("HLBFT_SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(200);
    let dur: u64 = std::env::var("HLBFT_SIM_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(300_000);
    let mut worst = 0;
    let mut failures = 0;
    for seed in 0..seeds {
        for n in [4, 7] {
            let base: u64 = std::env::var("HLBFT_SIM_BASE").ok().and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
            match run_schedule(n, base + seed, dur) {
                Ok(o) => {
                    worst = worst.max(o.max_resume_ms);
                    println!("seed {seed} n {n}: {o:?}");
                }
                Err(e) => {
                    failures += 1;
                    println!("FAIL {}", e.lines().next().unwrap_or(""));
                }
            }
        }
    }
    println!("worst resume: {worst} ms, failures: {failures}");
    assert_eq!(failures, 0);
}

/// Reproduce one schedule with logs:
/// `HLBFT_SEED=.. HLBFT_N=7 RUST_LOG=info cargo test -p consensus --test sim_prop debug_one_seed -- --ignored --nocapture`
#[test]
#[ignore]
fn debug_one_seed() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_ansi(false)
        .with_test_writer()
        .try_init();
    let seed: u64 = std::env::var("HLBFT_SEED").unwrap().parse().unwrap();
    let n: usize = std::env::var("HLBFT_N").unwrap().parse().unwrap();
    let dur: u64 = std::env::var("HLBFT_SIM_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(300_000);
    let o = run_schedule(n, seed, dur).unwrap_or_else(|e| panic!("{e}"));
    println!("{o:?}");
}
