//! Final / partial JSON report (TEST_PLAN AC1 evidence fields + details).

use crate::checker::{CheckerStats, Violation, ViolationKind};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug, Serialize)]
pub struct HealRecord {
    pub episode: u64,
    pub kind: String,
    pub halting: bool,
    /// Wall-clock offset of the heal from run start.
    pub at_s: f64,
    pub base_height: u64,
    /// Time from heal until max committed height advanced ≥ 2 blocks
    /// (None if it never did within the observation cap).
    pub resume_ms: Option<u64>,
    /// Time until every node is within `catchup_slack` blocks of the max.
    pub catchup_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EpisodeRecord {
    pub id: u64,
    pub kind: String,
    pub desc: String,
    pub start_s: f64,
    pub duration_ms: u64,
    pub halting: bool,
    pub height_start: u64,
    pub height_end: u64,
    pub stalls: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub duration_s: u64,
    pub validators: usize,
    pub max_faulty: usize,
    pub seed: u64,
    pub episodes: u64,
    pub heals: u64,
    pub max_resume_ms: u64,
    pub p50_resume_ms: u64,
    pub p99_resume_ms: u64,
    pub resume_limit_ms: u64,
    pub safety_violations: u64,
    pub kills: u64,
    pub restarts: u64,
    pub partitions: u64,
    pub splits: u64,
    pub lossy_episodes: u64,
    pub progress_stalls: u64,
    pub unexpected_exits: u64,
    pub final_height: u64,
    pub load: BTreeMap<String, u64>,
    pub episodes_by_kind: BTreeMap<String, u64>,
    pub violations_by_kind: BTreeMap<ViolationKind, u64>,
    pub violations: Vec<Violation>,
    pub checker: CheckerStats,
    pub heal_records: Vec<HealRecord>,
    pub episode_records: Vec<EpisodeRecord>,
    /// Harness-level problems (startup failures etc.).
    pub errors: Vec<String>,
    /// false while the run is in progress (partial flush).
    pub complete: bool,
    pub interrupted: bool,
    pub pass: bool,
    pub summary: String,
}

/// Nearest-rank percentile of `v` (sorted copy); 0 for empty.
pub fn percentile(v: &[u64], p: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let rank = ((p / 100.0) * s.len() as f64).ceil() as usize;
    s[rank.clamp(1, s.len()) - 1]
}

impl Report {
    /// Recompute derived fields from the records.
    pub fn finalize_stats(&mut self) {
        let r: Vec<u64> = self.heal_records.iter().map(|h| h.resume_ms.unwrap_or(u64::MAX / 4)).collect();
        self.heals = self.heal_records.len() as u64;
        self.max_resume_ms = r.iter().copied().max().unwrap_or(0);
        self.p50_resume_ms = percentile(&r, 50.0);
        self.p99_resume_ms = percentile(&r, 99.0);
    }

    /// Atomic write (tmp + rename) so a crash never leaves half a report.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).expect("report json"))?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        assert_eq!(percentile(&[], 50.0), 0);
        assert_eq!(percentile(&[5], 99.0), 5);
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&v, 50.0), 50);
        assert_eq!(percentile(&v, 99.0), 99);
        assert_eq!(percentile(&v, 100.0), 100);
        assert_eq!(percentile(&[3, 1, 2], 50.0), 2);
    }

    #[test]
    fn write_is_valid_json() {
        let d = crate::testutil::tempdir("report");
        let mut r = Report { validators: 7, ..Default::default() };
        r.heal_records.push(HealRecord { episode: 1, kind: "kill".into(), halting: false, at_s: 1.0, base_height: 3, resume_ms: Some(700), catchup_ms: None });
        r.finalize_stats();
        let p = d.join("r.json");
        r.write(&p).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["validators"], 7);
        assert_eq!(v["max_resume_ms"], 700);
        assert_eq!(v["heals"], 1);
    }
}
