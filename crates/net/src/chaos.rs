//! Fault-injection shim (test / acceptance harness only).
//!
//! Applied per destination on the *outbound* path (drop, delay, reorder,
//! duplicate, partitions, per-link block) and, for partitions and blocked
//! links only, also on the *inbound* path, so a single node's config is
//! enough to fully isolate it ("both directions cut").
//!
//! Partition semantics: if `partitions` is non-empty, traffic flows only
//! between nodes listed in the same group. A node listed in no group is
//! isolated from everyone. `partitions: []` = fully connected.
//!
//! Changes take effect immediately: queued and delayed messages are
//! re-checked against the *current* partition/block rules just before being
//! written to the socket, so a partition cuts traffic instantly, and a heal
//! lets new traffic through instantly.

use interfaces::Address;
use parking_lot::{Mutex, RwLock};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

fn default_reorder_ms() -> u64 {
    100
}

/// Runtime chaos configuration (JSON via serde; all fields optional).
///
/// ```json
/// {"partitions": [["0xaa..","0xbb.."],["0xcc..","0xdd.."]],
///  "drop_prob": 0.1, "delay_ms_min": 0, "delay_ms_max": 500,
///  "reorder_prob": 0.05, "reorder_delay_ms_max": 100, "duplicate_prob": 0.01,
///  "links": [{"to": "0xcc..", "blocked": true}], "seed": 42}
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChaosConfig {
    /// Groups of addresses; traffic only within a group. Empty = no partition.
    pub partitions: Vec<Vec<Address>>,
    /// Probability that an outbound message is dropped.
    pub drop_prob: f64,
    /// Every outbound message is delayed uniformly in [min, max] ms.
    pub delay_ms_min: u64,
    pub delay_ms_max: u64,
    /// Probability that a message gets an additional uniform
    /// [0, reorder_delay_ms_max] ms delay (makes it overtake / be overtaken).
    pub reorder_prob: f64,
    pub reorder_delay_ms_max: u64,
    /// Probability that a message is sent twice (independent delays).
    pub duplicate_prob: f64,
    /// Per-destination overrides.
    pub links: Vec<LinkChaos>,
    /// Reseed the chaos RNG (mixed with the local address) when applied.
    pub seed: Option<u64>,
}

impl Default for ChaosConfig {
    fn default() -> Self {
        Self {
            partitions: vec![],
            drop_prob: 0.0,
            delay_ms_min: 0,
            delay_ms_max: 0,
            reorder_prob: 0.0,
            reorder_delay_ms_max: default_reorder_ms(),
            duplicate_prob: 0.0,
            links: vec![],
            seed: None,
        }
    }
}

/// Overrides for traffic to one destination. `None` = use the global value.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinkChaos {
    pub to: Address,
    /// Cut this link (both directions, like a partition).
    pub blocked: bool,
    pub drop_prob: Option<f64>,
    pub delay_ms_min: Option<u64>,
    pub delay_ms_max: Option<u64>,
    pub reorder_prob: Option<f64>,
    pub duplicate_prob: Option<f64>,
}

impl ChaosConfig {
    pub fn from_json(s: &str) -> Result<Self, String> {
        let c: ChaosConfig = serde_json::from_str(s).map_err(|e| format!("bad chaos json: {e}"))?;
        c.validate()?;
        Ok(c)
    }

    /// Convenience: isolate `node` from everyone in `all`.
    pub fn isolate(node: Address, all: &[Address]) -> Self {
        let rest: Vec<Address> = all.iter().copied().filter(|a| *a != node).collect();
        Self { partitions: vec![vec![node], rest], ..Default::default() }
    }

    pub fn validate(&self) -> Result<(), String> {
        let p = |name: &str, v: f64| {
            if (0.0..=1.0).contains(&v) {
                Ok(())
            } else {
                Err(format!("{name} must be in [0,1], got {v}"))
            }
        };
        p("drop_prob", self.drop_prob)?;
        p("reorder_prob", self.reorder_prob)?;
        p("duplicate_prob", self.duplicate_prob)?;
        if self.delay_ms_min > self.delay_ms_max {
            return Err("delay_ms_min > delay_ms_max".into());
        }
        for l in &self.links {
            if let Some(v) = l.drop_prob {
                p("links.drop_prob", v)?;
            }
            if let Some(v) = l.reorder_prob {
                p("links.reorder_prob", v)?;
            }
            if let Some(v) = l.duplicate_prob {
                p("links.duplicate_prob", v)?;
            }
            let lo = l.delay_ms_min.unwrap_or(self.delay_ms_min);
            let hi = l.delay_ms_max.unwrap_or(self.delay_ms_max);
            if lo > hi {
                return Err(format!("link {}: delay min > max", l.to));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct LinkParams {
    drop_prob: f64,
    delay_min: u64,
    delay_max: u64,
    reorder_prob: f64,
    reorder_max: u64,
    duplicate_prob: f64,
}

impl LinkParams {
    fn is_noop(&self) -> bool {
        self.drop_prob == 0.0 && self.delay_max == 0 && self.reorder_prob == 0.0 && self.duplicate_prob == 0.0
    }
}

/// Compiled form of a config.
struct Compiled {
    cfg: ChaosConfig,
    /// None = no partitions.
    my_group: Option<usize>,
    group_of: BTreeMap<Address, usize>,
    blocked: BTreeMap<Address, ()>,
    default: LinkParams,
    links: BTreeMap<Address, LinkParams>,
    /// No chaos at all: fast path.
    noop: bool,
}

impl Compiled {
    fn new(local: Address, cfg: ChaosConfig) -> Self {
        let mut group_of = BTreeMap::new();
        for (i, g) in cfg.partitions.iter().enumerate() {
            for a in g {
                group_of.insert(*a, i);
            }
        }
        let my_group = if cfg.partitions.is_empty() { None } else { Some(group_of.get(&local).copied().unwrap_or(usize::MAX)) };
        let default = LinkParams {
            drop_prob: cfg.drop_prob,
            delay_min: cfg.delay_ms_min,
            delay_max: cfg.delay_ms_max,
            reorder_prob: cfg.reorder_prob,
            reorder_max: cfg.reorder_delay_ms_max,
            duplicate_prob: cfg.duplicate_prob,
        };
        let mut links = BTreeMap::new();
        let mut blocked = BTreeMap::new();
        for l in &cfg.links {
            if l.blocked {
                blocked.insert(l.to, ());
            }
            links.insert(
                l.to,
                LinkParams {
                    drop_prob: l.drop_prob.unwrap_or(default.drop_prob),
                    delay_min: l.delay_ms_min.unwrap_or(default.delay_min),
                    delay_max: l.delay_ms_max.unwrap_or(default.delay_max),
                    reorder_prob: l.reorder_prob.unwrap_or(default.reorder_prob),
                    reorder_max: default.reorder_max,
                    duplicate_prob: l.duplicate_prob.unwrap_or(default.duplicate_prob),
                },
            );
        }
        let noop = my_group.is_none() && blocked.is_empty() && default.is_noop() && links.values().all(|l| l.is_noop());
        Self { cfg, my_group, group_of, blocked, default, links, noop }
    }

    fn cut(&self, peer: &Address) -> bool {
        if self.blocked.contains_key(peer) {
            return true;
        }
        match self.my_group {
            None => false,
            Some(g) => g == usize::MAX || self.group_of.get(peer) != Some(&g),
        }
    }
}

/// Outcome of the outbound chaos decision for one message to one peer:
/// a list of delays (ms), one per copy to send. Empty = dropped.
pub type Copies = smallvec_like::Copies;

/// Tiny inline vector (0..=2 copies) to avoid an allocation per message.
pub mod smallvec_like {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Copies {
        n: u8,
        d: [u64; 2],
    }
    impl Copies {
        pub fn none() -> Self {
            Self::default()
        }
        pub fn one(d: u64) -> Self {
            Self { n: 1, d: [d, 0] }
        }
        pub fn push(&mut self, d: u64) {
            self.d[self.n as usize] = d;
            self.n += 1;
        }
        pub fn as_slice(&self) -> &[u64] {
            &self.d[..self.n as usize]
        }
        pub fn is_empty(&self) -> bool {
            self.n == 0
        }
    }
}

/// Thread-safe chaos engine shared by all connection tasks.
pub struct Chaos {
    local: Address,
    state: RwLock<Arc<Compiled>>,
    rng: Mutex<ChaCha8Rng>,
}

fn mix_seed(seed: u64, local: &Address) -> u64 {
    let mut x = seed;
    for c in local.0.chunks(8) {
        let mut b = [0u8; 8];
        b[..c.len()].copy_from_slice(c);
        x = x.rotate_left(17) ^ u64::from_le_bytes(b).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    x
}

impl Chaos {
    pub fn new(local: Address, cfg: ChaosConfig) -> Self {
        let rng = match cfg.seed {
            Some(s) => ChaCha8Rng::seed_from_u64(mix_seed(s, &local)),
            None => ChaCha8Rng::seed_from_u64(rand::random()),
        };
        Self { local, state: RwLock::new(Arc::new(Compiled::new(local, cfg))), rng: Mutex::new(rng) }
    }

    pub fn set(&self, cfg: ChaosConfig) -> Result<(), String> {
        cfg.validate()?;
        if let Some(s) = cfg.seed {
            *self.rng.lock() = ChaCha8Rng::seed_from_u64(mix_seed(s, &self.local));
        }
        *self.state.write() = Arc::new(Compiled::new(self.local, cfg));
        Ok(())
    }

    pub fn get(&self) -> ChaosConfig {
        self.state.read().cfg.clone()
    }

    /// Is the link to/from `peer` cut by a partition or a blocked link?
    pub fn is_cut(&self, peer: &Address) -> bool {
        let s = self.state.read();
        !s.noop && s.cut(peer)
    }

    /// Decide what happens to one outbound message to `peer`.
    pub fn outbound(&self, peer: &Address) -> Copies {
        let s = self.state.read().clone();
        if s.noop {
            return Copies::one(0);
        }
        if s.cut(peer) {
            return Copies::none();
        }
        let p = s.links.get(peer).copied().unwrap_or(s.default);
        if p.is_noop() {
            return Copies::one(0);
        }
        let mut rng = self.rng.lock();
        if p.drop_prob > 0.0 && rng.gen::<f64>() < p.drop_prob {
            return Copies::none();
        }
        let n = if p.duplicate_prob > 0.0 && rng.gen::<f64>() < p.duplicate_prob { 2 } else { 1 };
        let mut out = Copies::none();
        for _ in 0..n {
            let mut d = if p.delay_max > 0 { rng.gen_range(p.delay_min..=p.delay_max) } else { 0 };
            if p.reorder_prob > 0.0 && rng.gen::<f64>() < p.reorder_prob {
                d += rng.gen_range(0..=p.reorder_max);
            }
            out.push(d);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(i: u8) -> Address {
        Address([i; 20])
    }

    #[test]
    fn json_roundtrip_and_validation() {
        let c = ChaosConfig::from_json(&format!(
            r#"{{"partitions":[["{}","{}"],["{}"]],"drop_prob":0.25,"links":[{{"to":"{}","blocked":true}}],"seed":7}}"#,
            a(1),
            a(2),
            a(3),
            a(4)
        ))
        .unwrap();
        assert_eq!(c.partitions.len(), 2);
        assert_eq!(c.drop_prob, 0.25);
        assert_eq!(c.reorder_delay_ms_max, 100);
        let back = ChaosConfig::from_json(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
        assert!(ChaosConfig::from_json(r#"{"drop_prob": 1.5}"#).is_err());
        assert!(ChaosConfig::from_json(r#"{"delay_ms_min": 5, "delay_ms_max": 1}"#).is_err());
        assert!(ChaosConfig::from_json(r#"{"bogus": 1}"#).is_err());
        assert_eq!(ChaosConfig::from_json("{}").unwrap(), ChaosConfig::default());
    }

    #[test]
    fn partitions_cut() {
        let ch = Chaos::new(a(1), ChaosConfig::default());
        assert!(!ch.is_cut(&a(2)));
        assert_eq!(ch.outbound(&a(2)).as_slice(), &[0]);
        ch.set(ChaosConfig { partitions: vec![vec![a(1), a(2)], vec![a(3)]], ..Default::default() }).unwrap();
        assert!(!ch.is_cut(&a(2)));
        assert!(ch.is_cut(&a(3)));
        assert!(ch.is_cut(&a(4)), "unlisted node is not in my group");
        assert!(ch.outbound(&a(3)).is_empty());
        // local not listed => isolated
        ch.set(ChaosConfig { partitions: vec![vec![a(2), a(3)]], ..Default::default() }).unwrap();
        assert!(ch.is_cut(&a(2)) && ch.is_cut(&a(3)));
        ch.set(ChaosConfig::isolate(a(1), &[a(1), a(2), a(3)])).unwrap();
        assert!(ch.is_cut(&a(2)));
        ch.set(ChaosConfig::default()).unwrap();
        assert!(!ch.is_cut(&a(2)));
        // blocked link
        ch.set(ChaosConfig { links: vec![LinkChaos { to: a(3), blocked: true, ..Default::default() }], ..Default::default() })
            .unwrap();
        assert!(ch.is_cut(&a(3)) && !ch.is_cut(&a(2)));
    }

    #[test]
    fn probabilities_seeded_and_deterministic() {
        let cfg = ChaosConfig {
            drop_prob: 0.3,
            delay_ms_min: 10,
            delay_ms_max: 20,
            duplicate_prob: 0.1,
            reorder_prob: 0.2,
            reorder_delay_ms_max: 50,
            seed: Some(1),
            ..Default::default()
        };
        let run = || {
            let ch = Chaos::new(a(1), cfg.clone());
            (0..10_000).map(|_| ch.outbound(&a(2))).collect::<Vec<_>>()
        };
        let r1 = run();
        assert_eq!(r1, run());
        let dropped = r1.iter().filter(|c| c.is_empty()).count();
        assert!((2500..3500).contains(&dropped), "dropped {dropped}");
        let dup = r1.iter().filter(|c| c.as_slice().len() == 2).count();
        assert!((400..1000).contains(&dup), "dup {dup}");
        for c in &r1 {
            for d in c.as_slice() {
                assert!((10..=70).contains(d));
            }
        }
        assert!(r1.iter().any(|c| c.as_slice().iter().any(|d| *d > 20)), "reorder adds delay");
    }

    #[test]
    fn link_override() {
        let ch = Chaos::new(
            a(1),
            ChaosConfig { links: vec![LinkChaos { to: a(3), drop_prob: Some(1.0), ..Default::default() }], ..Default::default() },
        );
        assert!(ch.outbound(&a(3)).is_empty());
        assert_eq!(ch.outbound(&a(2)).as_slice(), &[0]);
    }
}
