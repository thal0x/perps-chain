//! Seeded fault-episode scheduler and the network-plan → per-node
//! `ChaosConfig` JSON translation.
//!
//! An [`Episode`] is a list of timed [`Step`]s (kill / restart / set network
//! plan) followed by a heal (all chaos cleared, every killed node restarted)
//! and an observation window. The *faulty set* at any instant is
//!
//! * killed nodes,
//! * nodes with a lossy/slow link fault ([`NetPlan::lossy`]),
//! * nodes outside the quorum group of a partition (or every node, if no
//!   group has a quorum — a deliberate whole-network split).
//!
//! Except for episodes marked `halting` (whole-network splits, which contain
//! nothing else), `|faulty| <= max_faulty` holds at every instant
//! ([`check_episode`], property-tested).

use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Per-node link degradation, applied to all of the node's links in both
/// directions.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LinkFault {
    pub drop_prob: f64,
    pub delay_ms_min: u64,
    pub delay_ms_max: u64,
    pub reorder_prob: f64,
    pub reorder_delay_ms_max: u64,
    pub duplicate_prob: f64,
}

/// Benign network-wide jitter (well within partial-synchrony assumptions;
/// not counted as a fault): small delay, reordering and duplication, no drop.
pub const BENIGN_MAX_DELAY_MS: u64 = 100;

impl LinkFault {
    pub fn is_benign(&self) -> bool {
        self.drop_prob == 0.0 && self.delay_ms_max <= BENIGN_MAX_DELAY_MS && self.reorder_delay_ms_max <= BENIGN_MAX_DELAY_MS
    }
}

/// Desired network state (node indices 0..n).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct NetPlan {
    /// Partition groups (every node in exactly one group); empty = connected.
    pub groups: Vec<Vec<usize>>,
    /// Lossy / slow nodes.
    pub lossy: BTreeMap<usize, LinkFault>,
    /// Network-wide benign jitter.
    pub jitter: Option<LinkFault>,
}

impl NetPlan {
    pub fn is_healed(&self) -> bool {
        self.groups.is_empty() && self.lossy.is_empty() && self.jitter.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Action {
    Kill { node: usize },
    Restart { node: usize },
    Net { plan: NetPlan },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Step {
    pub at_ms: u64,
    pub action: Action,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeKind {
    Kill,
    RollingKill,
    Isolate,
    MinorityPartition,
    Lossy,
    Jitter,
    Combo,
    Split,
}

impl EpisodeKind {
    pub const ALL: [EpisodeKind; 8] = [
        EpisodeKind::Kill,
        EpisodeKind::RollingKill,
        EpisodeKind::Isolate,
        EpisodeKind::MinorityPartition,
        EpisodeKind::Lossy,
        EpisodeKind::Jitter,
        EpisodeKind::Combo,
        EpisodeKind::Split,
    ];
    pub fn name(&self) -> &'static str {
        match self {
            EpisodeKind::Kill => "kill",
            EpisodeKind::RollingKill => "rolling_kill",
            EpisodeKind::Isolate => "isolate",
            EpisodeKind::MinorityPartition => "minority_partition",
            EpisodeKind::Lossy => "lossy",
            EpisodeKind::Jitter => "jitter",
            EpisodeKind::Combo => "combo",
            EpisodeKind::Split => "split",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.name() == s)
    }
    /// Relative scheduling weight.
    fn weight(&self) -> u32 {
        match self {
            EpisodeKind::Kill => 18,
            EpisodeKind::RollingKill => 8,
            EpisodeKind::Isolate => 14,
            EpisodeKind::MinorityPartition => 14,
            EpisodeKind::Lossy => 14,
            EpisodeKind::Jitter => 6,
            EpisodeKind::Combo => 14,
            EpisodeKind::Split => 12,
        }
    }
    /// Needs at least one faulty slot.
    fn needs_faulty(&self) -> bool {
        !matches!(self, EpisodeKind::Jitter | EpisodeKind::Split)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Episode {
    pub id: u64,
    pub kind: EpisodeKind,
    pub duration_ms: u64,
    /// Whole-network split: progress is expected to halt (exempt from the
    /// max-faulty invariant).
    pub halting: bool,
    pub steps: Vec<Step>,
    /// Human-readable one-liner.
    pub desc: String,
}

impl Episode {
    pub fn kills(&self) -> usize {
        self.steps.iter().filter(|s| matches!(s.action, Action::Kill { .. })).count()
    }
    pub fn has_partition(&self) -> bool {
        self.steps.iter().any(|s| matches!(&s.action, Action::Net { plan } if !plan.groups.is_empty()))
    }
}

/// Equal-stake quorum size: floor(2n/3) + 1.
pub fn quorum(n: usize) -> usize {
    2 * n / 3 + 1
}

/// Faulty nodes due to a partition, or `None` if no group holds a quorum.
pub fn partition_faulty(n: usize, groups: &[Vec<usize>]) -> Option<BTreeSet<usize>> {
    if groups.is_empty() {
        return Some(BTreeSet::new());
    }
    let q = quorum(n);
    let big = groups.iter().find(|g| g.len() >= q)?;
    Some((0..n).filter(|i| !big.contains(i)).collect())
}

/// Faulty set for (killed, plan); `None` = quorumless whole-network split.
pub fn faulty_set(n: usize, killed: &BTreeSet<usize>, plan: &NetPlan) -> Option<BTreeSet<usize>> {
    let mut f = partition_faulty(n, &plan.groups)?;
    f.extend(killed.iter().copied());
    f.extend(plan.lossy.keys().copied());
    if let Some(j) = &plan.jitter {
        if !j.is_benign() {
            f.extend(0..n);
        }
    }
    Some(f)
}

/// Validate an episode: steps sorted and within the duration, kills target
/// live nodes, restarts target killed nodes, partitions cover every node
/// exactly once, and the max-faulty invariant holds after every step (halting
/// episodes: a single quorumless split, nothing else).
pub fn check_episode(n: usize, max_faulty: usize, ep: &Episode) -> Result<(), String> {
    let mut killed = BTreeSet::new();
    let mut plan = NetPlan::default();
    let mut last_at = 0;
    for (i, s) in ep.steps.iter().enumerate() {
        if s.at_ms < last_at || s.at_ms > ep.duration_ms {
            return Err(format!("step {i} at {} out of order / beyond duration {}", s.at_ms, ep.duration_ms));
        }
        last_at = s.at_ms;
        match &s.action {
            Action::Kill { node } => {
                if *node >= n || !killed.insert(*node) {
                    return Err(format!("step {i}: kill of dead/unknown node {node}"));
                }
            }
            Action::Restart { node } => {
                if !killed.remove(node) {
                    return Err(format!("step {i}: restart of live node {node}"));
                }
            }
            Action::Net { plan: p } => {
                if !p.groups.is_empty() {
                    let mut seen = BTreeSet::new();
                    for g in &p.groups {
                        if g.is_empty() {
                            return Err(format!("step {i}: empty group"));
                        }
                        for x in g {
                            if *x >= n || !seen.insert(*x) {
                                return Err(format!("step {i}: node {x} in several groups / unknown"));
                            }
                        }
                    }
                    if seen.len() != n {
                        return Err(format!("step {i}: partition does not cover all nodes"));
                    }
                }
                if p.lossy.keys().any(|k| *k >= n) {
                    return Err(format!("step {i}: lossy unknown node"));
                }
                plan = p.clone();
            }
        }
        match faulty_set(n, &killed, &plan) {
            None => {
                if !ep.halting {
                    return Err(format!("step {i}: quorumless split in a non-halting episode"));
                }
                if !killed.is_empty() || !plan.lossy.is_empty() {
                    return Err(format!("step {i}: halting split combined with other faults"));
                }
            }
            Some(f) => {
                if f.len() > max_faulty {
                    return Err(format!("step {i}: {} faulty nodes {:?} > max {max_faulty}", f.len(), f));
                }
                if ep.halting && i + 1 == ep.steps.len() {
                    return Err("halting episode never splits the network".into());
                }
            }
        }
    }
    if ep.halting && partition_faulty(n, &plan.groups).is_some() {
        return Err("halting episode does not end in a quorumless split".into());
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct SchedulerConfig {
    pub n: usize,
    pub max_faulty: usize,
    pub min_ms: u64,
    pub max_ms: u64,
    pub kinds: Vec<EpisodeKind>,
}

pub struct Scheduler {
    cfg: SchedulerConfig,
    rng: ChaCha8Rng,
    next_id: u64,
}

fn pick_nodes(rng: &mut ChaCha8Rng, n: usize, k: usize) -> Vec<usize> {
    let mut all: Vec<usize> = (0..n).collect();
    all.shuffle(rng);
    all.truncate(k);
    all
}

fn rand_fault(rng: &mut ChaCha8Rng) -> LinkFault {
    // per-link delay 0–2000 ms, drop 0–30 %, reordering, duplication
    let a = rng.gen_range(0..=2000u64);
    let b = rng.gen_range(0..=2000u64);
    let style = rng.gen_range(0..4);
    let (drop_prob, (dmin, dmax)) = match style {
        0 => (0.0, (a.min(b), a.max(b))),                        // slow only
        1 => (rng.gen_range(0..=30) as f64 / 100.0, (0, 0)),     // lossy only
        _ => (rng.gen_range(0..=30) as f64 / 100.0, (a.min(b) / 4, a.max(b))), // both
    };
    LinkFault {
        drop_prob,
        delay_ms_min: dmin,
        delay_ms_max: dmax,
        reorder_prob: if rng.gen_bool(0.6) { rng.gen_range(0..=50) as f64 / 100.0 } else { 0.0 },
        reorder_delay_ms_max: rng.gen_range(10..=500),
        duplicate_prob: if rng.gen_bool(0.4) { rng.gen_range(0..=20) as f64 / 100.0 } else { 0.0 },
    }
}

fn benign_jitter(rng: &mut ChaCha8Rng) -> LinkFault {
    let max = rng.gen_range(5..=BENIGN_MAX_DELAY_MS);
    LinkFault {
        drop_prob: 0.0,
        delay_ms_min: 0,
        delay_ms_max: max,
        reorder_prob: rng.gen_range(0..=30) as f64 / 100.0,
        reorder_delay_ms_max: rng.gen_range(5..=BENIGN_MAX_DELAY_MS),
        duplicate_prob: rng.gen_range(0..=10) as f64 / 100.0,
    }
}

/// Random partition of `0..n` into groups all smaller than the quorum
/// (2 or 3 groups).
pub fn quorumless_split(rng: &mut ChaCha8Rng, n: usize) -> Vec<Vec<usize>> {
    let q = quorum(n);
    loop {
        let k = if n >= 5 && rng.gen_bool(0.4) { 3 } else { 2 };
        let mut nodes: Vec<usize> = (0..n).collect();
        nodes.shuffle(rng);
        let mut groups: Vec<Vec<usize>> = vec![vec![]; k];
        for (i, x) in nodes.iter().enumerate() {
            // first k nodes seed each group, the rest go anywhere
            let g = if i < k { i } else { rng.gen_range(0..k) };
            groups[g].push(*x);
        }
        if groups.iter().all(|g| !g.is_empty() && g.len() < q) {
            for g in groups.iter_mut() {
                g.sort_unstable();
            }
            return groups;
        }
    }
}

fn isolate_groups(n: usize, nodes: &[usize]) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = nodes.iter().map(|x| vec![*x]).collect();
    groups.push((0..n).filter(|i| !nodes.contains(i)).collect());
    groups
}

impl Scheduler {
    pub fn new(cfg: SchedulerConfig, seed: u64) -> Self {
        assert!(cfg.n >= 2, "need at least 2 validators");
        assert!(cfg.min_ms <= cfg.max_ms && cfg.min_ms > 0);
        Self { cfg, rng: ChaCha8Rng::seed_from_u64(seed ^ 0x6368_616f_735f_7363), next_id: 1 }
    }

    fn usable_kinds(&self) -> Vec<EpisodeKind> {
        let f = self.cfg.max_faulty.min(self.cfg.n.saturating_sub(quorum(self.cfg.n)));
        let mut ks: Vec<EpisodeKind> = self
            .cfg
            .kinds
            .iter()
            .copied()
            .filter(|k| f > 0 || !k.needs_faulty())
            .filter(|k| *k != EpisodeKind::Combo || f >= 2)
            .filter(|k| *k != EpisodeKind::Split || self.cfg.n >= 2)
            .collect();
        if ks.is_empty() {
            ks.push(EpisodeKind::Jitter);
        }
        ks
    }

    /// Effective fault budget: never more than n - quorum (so non-halting
    /// episodes keep a live quorum).
    pub fn budget(&self) -> usize {
        self.cfg.max_faulty.min(self.cfg.n.saturating_sub(quorum(self.cfg.n)))
    }

    /// Next episode; `max_duration_ms` clamps the duration (end of run).
    pub fn next_episode(&mut self, max_duration_ms: Option<u64>) -> Episode {
        let kinds = self.usable_kinds();
        let total: u32 = kinds.iter().map(|k| k.weight()).sum();
        let mut r = self.rng.gen_range(0..total);
        let mut kind = kinds[0];
        for k in &kinds {
            if r < k.weight() {
                kind = *k;
                break;
            }
            r -= k.weight();
        }
        let mut d = self.rng.gen_range(self.cfg.min_ms..=self.cfg.max_ms);
        if let Some(m) = max_duration_ms {
            d = d.min(m).max(1);
        }
        let id = self.next_id;
        self.next_id += 1;
        let ep = self.build(id, kind, d);
        debug_assert!(check_episode(self.cfg.n, self.budget(), &ep).is_ok(), "{:?}", check_episode(self.cfg.n, self.budget(), &ep));
        ep
    }

    fn build(&mut self, id: u64, kind: EpisodeKind, d: u64) -> Episode {
        let n = self.cfg.n;
        let f = self.budget();
        let rng = &mut self.rng;
        let mut steps = vec![];
        let mut halting = false;
        let desc;
        match kind {
            EpisodeKind::Kill => {
                let k = rng.gen_range(1..=f);
                let nodes = pick_nodes(rng, n, k);
                for (j, x) in nodes.iter().enumerate() {
                    steps.push(Step { at_ms: if j == 0 { 0 } else { rng.gen_range(0..=d / 5) }, action: Action::Kill { node: *x } });
                }
                // restart some mid-episode, the rest at heal
                for x in &nodes {
                    if rng.gen_bool(0.5) {
                        steps.push(Step { at_ms: rng.gen_range(d * 3 / 10..=d), action: Action::Restart { node: *x } });
                    }
                }
                desc = format!("kill -9 {:?}", nodes);
            }
            EpisodeKind::RollingKill => {
                // one node down at a time, cycling through random nodes
                let mut t = 0u64;
                let mut victims = vec![];
                while t + 4_000 <= d {
                    let x = rng.gen_range(0..n);
                    let down = rng.gen_range(2_000..=(d - t).min(15_000).max(2_000));
                    steps.push(Step { at_ms: t, action: Action::Kill { node: x } });
                    let up = (t + down).min(d);
                    steps.push(Step { at_ms: up, action: Action::Restart { node: x } });
                    victims.push(x);
                    t = up + rng.gen_range(1_000..=5_000);
                }
                if steps.is_empty() {
                    let x = rng.gen_range(0..n);
                    steps.push(Step { at_ms: 0, action: Action::Kill { node: x } });
                    victims.push(x);
                }
                desc = format!("rolling kill -9 {:?}", victims);
            }
            EpisodeKind::Isolate => {
                let k = rng.gen_range(1..=f);
                let nodes = pick_nodes(rng, n, k);
                steps.push(Step { at_ms: 0, action: Action::Net { plan: NetPlan { groups: isolate_groups(n, &nodes), ..Default::default() } } });
                desc = format!("isolate {:?}", nodes);
            }
            EpisodeKind::MinorityPartition => {
                let k = rng.gen_range(1..=f);
                let minority = pick_nodes(rng, n, k);
                let majority: Vec<usize> = (0..n).filter(|i| !minority.contains(i)).collect();
                let mut groups = vec![majority];
                if k >= 2 && rng.gen_bool(0.5) {
                    for x in &minority {
                        groups.push(vec![*x]);
                    }
                } else {
                    let mut m = minority.clone();
                    m.sort_unstable();
                    groups.push(m);
                }
                if rng.gen_bool(0.5) {
                    groups.reverse();
                }
                let sizes: Vec<String> = groups.iter().map(|g| g.len().to_string()).collect();
                desc = format!("partition {} {:?}", sizes.join("|"), groups);
                steps.push(Step { at_ms: 0, action: Action::Net { plan: NetPlan { groups, ..Default::default() } } });
            }
            EpisodeKind::Lossy => {
                let k = rng.gen_range(1..=f);
                let nodes = pick_nodes(rng, n, k);
                let mut lossy = BTreeMap::new();
                for x in &nodes {
                    lossy.insert(*x, rand_fault(rng));
                }
                let jitter = if rng.gen_bool(0.3) { Some(benign_jitter(rng)) } else { None };
                desc = format!("lossy {:?}{}", lossy, if jitter.is_some() { " + jitter" } else { "" });
                steps.push(Step { at_ms: 0, action: Action::Net { plan: NetPlan { lossy, jitter, groups: vec![] } } });
            }
            EpisodeKind::Jitter => {
                let j = benign_jitter(rng);
                desc = format!("network-wide jitter {:?}", j);
                steps.push(Step { at_ms: 0, action: Action::Net { plan: NetPlan { jitter: Some(j), ..Default::default() } } });
            }
            EpisodeKind::Combo => {
                // f >= 2: one node killed + up to f-1 others isolated or lossy
                let nodes = pick_nodes(rng, n, f);
                let victim = nodes[0];
                let others = &nodes[1..];
                let mut plan = NetPlan::default();
                let what;
                if rng.gen_bool(0.5) {
                    plan.groups = isolate_groups(n, others);
                    what = "isolate";
                } else {
                    for x in others {
                        plan.lossy.insert(*x, rand_fault(rng));
                    }
                    what = "lossy";
                }
                if rng.gen_bool(0.3) {
                    plan.jitter = Some(benign_jitter(rng));
                }
                let kill_at = rng.gen_range(0..=d / 3);
                steps.push(Step { at_ms: 0, action: Action::Net { plan } });
                steps.push(Step { at_ms: kill_at, action: Action::Kill { node: victim } });
                if rng.gen_bool(0.5) {
                    steps.push(Step { at_ms: rng.gen_range(kill_at..=d), action: Action::Restart { node: victim } });
                }
                desc = format!("combo kill {victim} + {what} {:?}", others);
            }
            EpisodeKind::Split => {
                let groups = quorumless_split(rng, n);
                let sizes: Vec<String> = groups.iter().map(|g| g.len().to_string()).collect();
                desc = format!("whole-network split {} {:?}", sizes.join("|"), groups);
                steps.push(Step { at_ms: 0, action: Action::Net { plan: NetPlan { groups, ..Default::default() } } });
                halting = true;
            }
        }
        steps.sort_by_key(|s| s.at_ms);
        Episode { id, kind, duration_ms: d, halting, steps, desc }
    }
}

fn fault_fields(f: &LinkFault) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("drop_prob".into(), json!(f.drop_prob));
    m.insert("delay_ms_min".into(), json!(f.delay_ms_min));
    m.insert("delay_ms_max".into(), json!(f.delay_ms_max));
    m.insert("reorder_prob".into(), json!(f.reorder_prob));
    m.insert("duplicate_prob".into(), json!(f.duplicate_prob));
    m
}

/// `net::ChaosConfig` JSON for node `me` under `plan`. `addrs[i]` is node
/// i's validator address (hex). Partitions are given to every node (the
/// shim enforces them inbound and outbound). A lossy node degrades all its
/// outbound traffic; the other nodes degrade their link *to* it, so the
/// fault is bidirectional.
pub fn chaos_json(plan: &NetPlan, me: usize, addrs: &[String], seed: u64) -> Value {
    if plan.is_healed() {
        return json!({});
    }
    let mut m = serde_json::Map::new();
    if !plan.groups.is_empty() {
        let g: Vec<Vec<String>> = plan.groups.iter().map(|g| g.iter().map(|i| addrs[*i].clone()).collect()).collect();
        m.insert("partitions".into(), json!(g));
    }
    let mut reorder_ms = 0;
    if let Some(j) = &plan.jitter {
        m.extend(fault_fields(j));
        reorder_ms = j.reorder_delay_ms_max;
    }
    if let Some(f) = plan.lossy.get(&me) {
        m.extend(fault_fields(f));
        reorder_ms = reorder_ms.max(f.reorder_delay_ms_max);
    } else {
        let links: Vec<Value> = plan
            .lossy
            .iter()
            .map(|(peer, f)| {
                let mut l = fault_fields(f);
                l.insert("to".into(), json!(addrs[*peer]));
                Value::Object(l)
            })
            .collect();
        if !links.is_empty() {
            m.insert("links".into(), Value::Array(links));
        }
        if let Some(lf) = plan.lossy.values().map(|f| f.reorder_delay_ms_max).max() {
            reorder_ms = reorder_ms.max(lf);
        }
    }
    if reorder_ms > 0 {
        m.insert("reorder_delay_ms_max".into(), json!(reorder_ms));
    }
    m.insert("seed".into(), json!(seed));
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn cfg(n: usize, f: usize) -> SchedulerConfig {
        SchedulerConfig { n, max_faulty: f, min_ms: 20_000, max_ms: 120_000, kinds: EpisodeKind::ALL.to_vec() }
    }

    #[test]
    fn quorum_sizes() {
        assert_eq!(quorum(4), 3);
        assert_eq!(quorum(7), 5);
        assert_eq!(quorum(5), 4);
    }

    #[test]
    fn partition_classification() {
        assert_eq!(partition_faulty(7, &[vec![0, 1, 2, 3, 4], vec![5, 6]]), Some([5, 6].into()));
        assert_eq!(partition_faulty(7, &[vec![0, 1, 2], vec![3, 4, 5, 6]]), None);
        assert_eq!(partition_faulty(7, &[vec![0, 1], vec![2, 3], vec![4, 5, 6]]), None);
        assert_eq!(partition_faulty(4, &[vec![0, 1], vec![2, 3]]), None);
    }

    #[test]
    fn invariant_violations_are_caught() {
        let ep = |steps, halting| Episode { id: 1, kind: EpisodeKind::Kill, duration_ms: 1000, halting, steps, desc: String::new() };
        let k = |at, node| Step { at_ms: at, action: Action::Kill { node } };
        assert!(check_episode(7, 2, &ep(vec![k(0, 1), k(1, 2)], false)).is_ok());
        assert!(check_episode(7, 2, &ep(vec![k(0, 1), k(1, 2), k(2, 3)], false)).is_err());
        assert!(check_episode(7, 2, &ep(vec![k(0, 1), k(1, 1)], false)).is_err());
        let iso = Step { at_ms: 5, action: Action::Net { plan: NetPlan { groups: isolate_groups(7, &[4]), ..Default::default() } } };
        assert!(check_episode(7, 2, &ep(vec![k(0, 1), k(1, 2), iso.clone()], false)).is_err());
        assert!(check_episode(7, 2, &ep(vec![k(0, 1), iso], false)).is_ok());
        let split = Step { at_ms: 0, action: Action::Net { plan: NetPlan { groups: vec![vec![0, 1, 2], vec![3, 4, 5, 6]], ..Default::default() } } };
        assert!(check_episode(7, 2, &ep(vec![split.clone()], false)).is_err());
        assert!(check_episode(7, 2, &ep(vec![split.clone()], true)).is_ok());
        assert!(check_episode(7, 2, &ep(vec![split, k(1, 0)], true)).is_err());
    }

    #[test]
    fn scheduler_is_deterministic_and_covers_kinds() {
        let mut a = Scheduler::new(cfg(7, 2), 42);
        let mut b = Scheduler::new(cfg(7, 2), 42);
        let mut kinds = BTreeSet::new();
        for _ in 0..300 {
            let x = a.next_episode(None);
            assert_eq!(x, b.next_episode(None));
            kinds.insert(x.kind);
            assert!((20_000..=120_000).contains(&x.duration_ms));
        }
        assert_eq!(kinds.len(), EpisodeKind::ALL.len());
        let mut c = Scheduler::new(cfg(7, 2), 43);
        assert_ne!(a.next_episode(None), c.next_episode(None));
    }

    #[test]
    fn kind_filter_and_zero_budget() {
        let mut s = Scheduler::new(SchedulerConfig { kinds: vec![EpisodeKind::Split], ..cfg(7, 2) }, 1);
        for _ in 0..20 {
            let e = s.next_episode(None);
            assert_eq!(e.kind, EpisodeKind::Split);
            assert!(e.halting);
        }
        let mut s = Scheduler::new(cfg(7, 0), 1);
        for _ in 0..50 {
            let e = s.next_episode(None);
            assert!(matches!(e.kind, EpisodeKind::Jitter | EpisodeKind::Split));
        }
        // max_faulty above n - quorum is clamped
        let s = Scheduler::new(cfg(4, 3), 1);
        assert_eq!(s.budget(), 1);
    }

    #[test]
    fn chaos_json_shapes() {
        let addrs: Vec<String> = (0..4).map(|i| format!("0x{:040x}", i + 1)).collect();
        assert_eq!(chaos_json(&NetPlan::default(), 0, &addrs, 9), json!({}));
        let plan = NetPlan { groups: vec![vec![0], vec![1, 2, 3]], ..Default::default() };
        let j = chaos_json(&plan, 2, &addrs, 9);
        assert_eq!(j["partitions"], json!([[addrs[0]], [addrs[1], addrs[2], addrs[3]]]));
        let mut lossy = BTreeMap::new();
        lossy.insert(1, LinkFault { drop_prob: 0.2, delay_ms_min: 10, delay_ms_max: 900, reorder_prob: 0.1, reorder_delay_ms_max: 300, duplicate_prob: 0.05 });
        let plan = NetPlan { lossy, ..Default::default() };
        let me = chaos_json(&plan, 1, &addrs, 9);
        assert_eq!(me["drop_prob"], json!(0.2));
        assert_eq!(me["delay_ms_max"], json!(900));
        assert!(me.get("links").is_none());
        let other = chaos_json(&plan, 0, &addrs, 9);
        assert_eq!(other["links"][0]["to"], json!(addrs[1]));
        assert_eq!(other["links"][0]["drop_prob"], json!(0.2));
        assert_eq!(other["reorder_delay_ms_max"], json!(300));
        assert!(other.get("drop_prob").is_none());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn scheduler_respects_max_faulty(seed in any::<u64>(), n in 4usize..=7, f in 0usize..=3,
                                         min_s in 1u64..60, extra_s in 0u64..90) {
            let c = SchedulerConfig { n, max_faulty: f, min_ms: min_s * 1000, max_ms: (min_s + extra_s) * 1000, kinds: EpisodeKind::ALL.to_vec() };
            let mut s = Scheduler::new(c, seed);
            let budget = s.budget();
            prop_assert!(budget <= f && budget <= n - quorum(n));
            for _ in 0..60 {
                let e = s.next_episode(None);
                prop_assert!(check_episode(n, budget, &e).is_ok(), "{:?}: {:?}", check_episode(n, budget, &e), e);
                prop_assert!(e.kills() <= 64);
                if e.halting {
                    prop_assert_eq!(e.kind, EpisodeKind::Split);
                }
                for st in &e.steps {
                    if let Action::Net { plan } = &st.action {
                        for lf in plan.lossy.values() {
                            prop_assert!(lf.drop_prob <= 0.30 && lf.delay_ms_max <= 2000);
                        }
                        if let Some(j) = &plan.jitter { prop_assert!(j.is_benign()); }
                    }
                }
            }
        }

        #[test]
        fn splits_are_quorumless(seed in any::<u64>(), n in 4usize..=7) {
            let mut rng = ChaCha8Rng::seed_from_u64(seed);
            let g = quorumless_split(&mut rng, n);
            prop_assert!(partition_faulty(n, &g).is_none());
            let mut all: Vec<usize> = g.concat();
            all.sort_unstable();
            prop_assert_eq!(all, (0..n).collect::<Vec<_>>());
        }
    }
}
