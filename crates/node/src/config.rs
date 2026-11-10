//! `config.toml` (per node home) + `hlnode run` CLI overrides.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    /// Paths are relative to the node home.
    pub genesis: Option<String>,
    pub key: Option<String>,
    pub p2p_listen: Option<String>,
    pub api_listen: Option<String>,
    pub min_block_ms: Option<u64>,
    /// `off` | `every` | `<N>`.
    pub invariants: Option<String>,
    /// `synthetic:<seed>[:skew_bps=<i>]` | `fixed` | `file:<path>` | `none`.
    pub oracle_feed: Option<String>,
    pub oracle_interval_ms: Option<u64>,
    pub bridge_rpc: Option<String>,
    pub bridge_confirmations: Option<u64>,
    pub replay_log: Option<String>,
    pub log_level: Option<String>,
    /// Snapshot interval in heights (default 1000).
    pub snapshot_interval: Option<u64>,
    pub mempool: mempool::PoolConfig,
}

/// CLI flags of `hlnode run` (all optional; override config.toml).
#[derive(Clone, Debug, Default, clap::Args)]
pub struct RunArgs {
    #[arg(long)]
    pub home: PathBuf,
    #[arg(long)]
    pub genesis: Option<PathBuf>,
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub p2p_listen: Option<String>,
    #[arg(long)]
    pub api_listen: Option<String>,
    #[arg(long)]
    pub key: Option<PathBuf>,
    #[arg(long)]
    pub min_block_ms: Option<u64>,
    #[arg(long)]
    pub invariants: Option<String>,
    #[arg(long)]
    pub oracle_feed: Option<String>,
    #[arg(long)]
    pub bridge_rpc: Option<String>,
    #[arg(long)]
    pub replay_log: Option<PathBuf>,
    #[arg(long)]
    pub log_level: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invariants {
    Off,
    Every,
    EveryN(u64),
}

impl Invariants {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim() {
            "off" | "never" | "0" => Invariants::Off,
            "every" | "1" => Invariants::Every,
            n => Invariants::EveryN(n.parse().with_context(|| format!("bad --invariants {n}"))?),
        })
    }
    pub fn to_mode(&self) -> state::InvariantMode {
        match self {
            Invariants::Off => state::InvariantMode::Never,
            Invariants::Every => state::InvariantMode::EveryBlock,
            Invariants::EveryN(n) => state::InvariantMode::EveryN(*n),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeedSpec {
    None,
    Fixed,
    Synthetic { seed: u64, skew_bps: i64 },
    File(PathBuf),
}

impl FeedSpec {
    pub fn parse(s: &str, home: &Path) -> Result<Self> {
        let s = s.trim();
        if s == "none" || s == "off" {
            return Ok(FeedSpec::None);
        }
        if s == "fixed" {
            return Ok(FeedSpec::Fixed);
        }
        if let Some(p) = s.strip_prefix("file:") {
            return Ok(FeedSpec::File(resolve(home, p)));
        }
        if let Some(rest) = s.strip_prefix("synthetic") {
            let rest = rest.strip_prefix(':').unwrap_or(rest);
            let mut seed = 0u64;
            let mut skew_bps = 0i64;
            for (i, part) in rest.split(':').filter(|p| !p.is_empty()).enumerate() {
                if let Some(v) = part.strip_prefix("skew_bps=") {
                    skew_bps = v.parse().context("bad skew_bps")?;
                } else if i == 0 {
                    seed = part.parse().context("bad synthetic seed")?;
                } else {
                    bail!("bad oracle feed option {part}");
                }
            }
            return Ok(FeedSpec::Synthetic { seed, skew_bps });
        }
        bail!("unknown --oracle-feed {s}")
    }
}

pub fn resolve(home: &Path, p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        pb
    } else {
        home.join(pb)
    }
}

/// Fully resolved run configuration.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub home: PathBuf,
    pub genesis_path: PathBuf,
    pub key_path: PathBuf,
    pub p2p_listen: Option<String>,
    pub api_listen: String,
    pub min_block_ms: u64,
    pub invariants: Invariants,
    pub oracle_feed: FeedSpec,
    pub oracle_interval_ms: u64,
    pub bridge_rpc: Option<String>,
    pub bridge_confirmations: u64,
    pub replay_log: Option<PathBuf>,
    pub log_level: String,
    pub snapshot_interval: u64,
    pub mempool: mempool::PoolConfig,
}

impl Resolved {
    pub fn from_args(a: &RunArgs) -> Result<Self> {
        let home = a.home.clone();
        let cfg_path = a.config.clone().unwrap_or_else(|| home.join("config.toml"));
        let cfg: NodeConfig = if cfg_path.exists() {
            let s = std::fs::read_to_string(&cfg_path).with_context(|| format!("read {}", cfg_path.display()))?;
            toml::from_str(&s).with_context(|| format!("parse {}", cfg_path.display()))?
        } else if a.config.is_some() {
            bail!("config {} not found", cfg_path.display())
        } else {
            NodeConfig::default()
        };
        let genesis_path = a
            .genesis
            .clone()
            .or_else(|| cfg.genesis.as_deref().map(|p| resolve(&home, p)))
            .unwrap_or_else(|| home.join("../genesis.json"));
        let key_path =
            a.key.clone().or_else(|| cfg.key.as_deref().map(|p| resolve(&home, p))).unwrap_or_else(|| home.join("validator.key"));
        let invariants = Invariants::parse(a.invariants.as_deref().or(cfg.invariants.as_deref()).unwrap_or("100"))?;
        let oracle_feed = FeedSpec::parse(a.oracle_feed.as_deref().or(cfg.oracle_feed.as_deref()).unwrap_or("synthetic:0"), &home)?;
        let replay_log = a.replay_log.clone().or_else(|| cfg.replay_log.as_deref().map(|p| resolve(&home, p)));
        Ok(Resolved {
            genesis_path,
            key_path,
            p2p_listen: a.p2p_listen.clone().or(cfg.p2p_listen.clone()),
            api_listen: a.api_listen.clone().or(cfg.api_listen.clone()).unwrap_or_else(|| "127.0.0.1:3001".into()),
            min_block_ms: a.min_block_ms.or(cfg.min_block_ms).unwrap_or(200),
            invariants,
            oracle_feed,
            oracle_interval_ms: cfg.oracle_interval_ms.unwrap_or(1000),
            bridge_rpc: a.bridge_rpc.clone().or(cfg.bridge_rpc.clone()).filter(|s| !s.is_empty()),
            bridge_confirmations: cfg.bridge_confirmations.unwrap_or(1),
            replay_log,
            log_level: a.log_level.clone().or(cfg.log_level.clone()).unwrap_or_else(|| "info".into()),
            snapshot_interval: cfg.snapshot_interval.unwrap_or(1000).max(1),
            mempool: cfg.mempool,
            home,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_feeds() {
        let h = Path::new("/h");
        assert_eq!(FeedSpec::parse("fixed", h).unwrap(), FeedSpec::Fixed);
        assert_eq!(FeedSpec::parse("synthetic:5", h).unwrap(), FeedSpec::Synthetic { seed: 5, skew_bps: 0 });
        assert_eq!(
            FeedSpec::parse("synthetic:5:skew_bps=-300", h).unwrap(),
            FeedSpec::Synthetic { seed: 5, skew_bps: -300 }
        );
        assert_eq!(FeedSpec::parse("file:p.txt", h).unwrap(), FeedSpec::File("/h/p.txt".into()));
        assert!(FeedSpec::parse("bogus", h).is_err());
        assert_eq!(Invariants::parse("off").unwrap(), Invariants::Off);
        assert_eq!(Invariants::parse("every").unwrap(), Invariants::Every);
        assert_eq!(Invariants::parse("50").unwrap(), Invariants::EveryN(50));
    }
    #[test]
    fn toml_roundtrip() {
        let c = NodeConfig { api_listen: Some("x:1".into()), ..Default::default() };
        let s = toml::to_string_pretty(&c).unwrap();
        let d: NodeConfig = toml::from_str(&s).unwrap();
        assert_eq!(d.api_listen.as_deref(), Some("x:1"));
        let e: NodeConfig = toml::from_str("").unwrap();
        assert!(e.api_listen.is_none());
    }
}
