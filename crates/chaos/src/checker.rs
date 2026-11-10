//! Continuous safety checker over every node's `commits.log`, `roots.log`
//! and log files (NODE_CLI.md):
//!
//! * `commits.log`: `<height> <block_id_hex> <timestamp_ms>`
//! * `roots.log`:   `<height> <state_root_hex> <order_actions>`
//! * `node.log` / console output: any `STATE DIVERGENCE` line.
//!
//! Violations: two nodes (or one node twice) committing different block ids
//! at the same height; different roots at an executed height; a node's
//! heights not contiguous (gap, unless `allow_gaps`); a divergence log line.
//! Re-logging the same `(height, id)` (e.g. after a crash-restart replay)
//! is fine and counted in `relogged`.

use crate::tail::LogTail;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ViolationKind {
    ConflictingCommit,
    RootMismatch,
    CommitGap,
    RootGap,
    StateDivergence,
    /// The chain advanced during a whole-network split where no group holds a
    /// quorum (recorded by the runner).
    ProgressWithoutQuorum,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Violation {
    pub kind: ViolationKind,
    pub node: usize,
    pub height: u64,
    pub detail: String,
}

/// Keep at most this many detailed violations (all are counted).
pub const MAX_VIOLATION_DETAILS: usize = 200;

#[derive(Debug, Default, Clone)]
struct Seq {
    /// Highest height seen from this node (contiguity is checked against it).
    last: Option<u64>,
}

#[derive(Debug)]
struct NodeFiles {
    commits: LogTail,
    roots: LogTail,
    logs: Vec<LogTail>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct CheckerStats {
    pub commit_lines: u64,
    pub root_lines: u64,
    pub relogged: u64,
    pub malformed_lines: u64,
    pub torn_lines: u64,
    pub max_committed: u64,
    pub max_executed: u64,
    /// Heights for which ≥ 2 nodes' block ids were compared.
    pub cross_checked_commits: u64,
    pub cross_checked_roots: u64,
}

#[derive(Debug)]
pub struct SafetyChecker {
    n: usize,
    allow_gaps: bool,
    /// height -> (block id, first node that reported it, number of nodes)
    commits: BTreeMap<u64, (String, usize, u32)>,
    roots: BTreeMap<u64, (String, usize, u32)>,
    commit_seq: Vec<Seq>,
    root_seq: Vec<Seq>,
    files: Option<Vec<NodeFiles>>,
    pub violations: Vec<Violation>,
    pub violation_count: u64,
    pub by_kind: BTreeMap<ViolationKind, u64>,
    pub stats: CheckerStats,
    /// Entries more than this far below every node's last height are pruned.
    pub retain: u64,
}

fn norm_hex(s: &str) -> Option<String> {
    let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if h.is_empty() || h.len() % 2 != 0 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(h.to_ascii_lowercase())
}

/// Parse `<height> <hex> <u64>`; None if malformed.
pub fn parse_line(line: &str) -> Option<(u64, String)> {
    let mut it = line.split_whitespace();
    let h: u64 = it.next()?.parse().ok()?;
    let id = norm_hex(it.next()?)?;
    let _third: u64 = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((h, id))
}

impl SafetyChecker {
    /// Pure checker (feed lines with `ingest_*`).
    pub fn new(n: usize, allow_gaps: bool) -> Self {
        Self {
            n,
            allow_gaps,
            commits: BTreeMap::new(),
            roots: BTreeMap::new(),
            commit_seq: vec![Seq::default(); n],
            root_seq: vec![Seq::default(); n],
            files: None,
            violations: vec![],
            violation_count: 0,
            by_kind: BTreeMap::new(),
            stats: CheckerStats::default(),
            retain: 20_000,
        }
    }

    /// Checker tailing `homes[i]/{commits.log,roots.log,node.log}` plus any
    /// extra log file names (e.g. the captured console output).
    pub fn with_homes(homes: &[impl AsRef<Path>], extra_logs: &[&str], allow_gaps: bool) -> Self {
        let mut c = Self::new(homes.len(), allow_gaps);
        c.files = Some(
            homes
                .iter()
                .map(|h| {
                    let h = h.as_ref();
                    let mut logs = vec![LogTail::new(h.join("node.log"))];
                    logs.extend(extra_logs.iter().map(|x| LogTail::new(h.join(x))));
                    NodeFiles { commits: LogTail::new(h.join("commits.log")), roots: LogTail::new(h.join("roots.log")), logs }
                })
                .collect(),
        );
        c
    }

    pub fn safety_violations(&self) -> u64 {
        self.violation_count
    }

    pub fn record(&mut self, v: Violation) {
        self.violation_count += 1;
        *self.by_kind.entry(v.kind).or_default() += 1;
        if self.violations.len() < MAX_VIOLATION_DETAILS {
            self.violations.push(v);
        }
    }

    /// Per-node highest committed height seen in the logs.
    pub fn node_committed(&self, node: usize) -> Option<u64> {
        self.commit_seq[node].last
    }
    pub fn node_executed(&self, node: usize) -> Option<u64> {
        self.root_seq[node].last
    }

    fn ingest(&mut self, node: usize, line: &str, is_root: bool) {
        if line.trim().is_empty() {
            return;
        }
        let Some((h, id)) = parse_line(line) else {
            self.stats.malformed_lines += 1;
            return;
        };
        if is_root {
            self.stats.root_lines += 1;
            self.stats.max_executed = self.stats.max_executed.max(h);
        } else {
            self.stats.commit_lines += 1;
            self.stats.max_committed = self.stats.max_committed.max(h);
        }
        let (map, seq) = if is_root { (&mut self.roots, &mut self.root_seq) } else { (&mut self.commits, &mut self.commit_seq) };
        let mut found: Option<Violation> = None;
        let what = if is_root { "root" } else { "block_id" };
        match map.get_mut(&h) {
            Some((prev, first, count)) => {
                if *prev != id {
                    found = Some(Violation {
                        kind: if is_root { ViolationKind::RootMismatch } else { ViolationKind::ConflictingCommit },
                        node,
                        height: h,
                        detail: format!("node{node} {what} {id} != node{first} {what} {prev} at height {h}"),
                    });
                } else {
                    *count += 1;
                    if *count == 2 {
                        if is_root {
                            self.stats.cross_checked_roots += 1;
                        } else {
                            self.stats.cross_checked_commits += 1;
                        }
                    }
                }
            }
            None => {
                map.insert(h, (id.clone(), node, 1));
            }
        }
        let s = &mut seq[node];
        let expected = s.last.map(|l| l + 1).unwrap_or(1);
        let mut gap: Option<Violation> = None;
        if h < expected {
            // re-log of an earlier height (restart replay); id compared above
            self.stats.relogged += 1;
        } else {
            if h > expected && !self.allow_gaps {
                gap = Some(Violation {
                    kind: if is_root { ViolationKind::RootGap } else { ViolationKind::CommitGap },
                    node,
                    height: h,
                    detail: format!("node{node} {} heights jump from {:?} to {h}", if is_root { "executed" } else { "committed" }, s.last),
                });
            }
            s.last = Some(h);
        }
        if let Some(v) = found {
            self.record(v);
        }
        if let Some(v) = gap {
            self.record(v);
        }
    }

    pub fn ingest_commit_line(&mut self, node: usize, line: &str) {
        self.ingest(node, line, false)
    }
    pub fn ingest_root_line(&mut self, node: usize, line: &str) {
        self.ingest(node, line, true)
    }
    pub fn ingest_log_line(&mut self, node: usize, line: &str) {
        if line.contains("STATE DIVERGENCE") {
            let detail: String = line.chars().take(400).collect();
            self.record(Violation { kind: ViolationKind::StateDivergence, node, height: 0, detail });
        }
    }

    /// Read everything new from the tailed files.
    pub fn poll(&mut self) {
        let Some(mut files) = self.files.take() else { return };
        for (i, f) in files.iter_mut().enumerate() {
            for l in f.commits.poll() {
                self.ingest_commit_line(i, &l);
            }
            for l in f.roots.poll() {
                self.ingest_root_line(i, &l);
            }
            for lt in f.logs.iter_mut() {
                for l in lt.poll() {
                    self.ingest_log_line(i, &l);
                }
            }
        }
        self.files = Some(files);
        self.prune();
    }

    /// Call after node `i` was killed (and is dead): drain its files and
    /// drop torn last lines so the restarted process's appends start clean.
    pub fn on_killed(&mut self, i: usize) {
        self.poll();
        if let Some(files) = self.files.as_mut() {
            let f = &mut files[i];
            let mut torn = 0;
            if f.commits.discard_partial().is_some() {
                torn += 1;
            }
            if f.roots.discard_partial().is_some() {
                torn += 1;
            }
            for l in f.logs.iter_mut() {
                l.discard_partial();
            }
            self.stats.torn_lines += torn;
        }
    }

    fn prune(&mut self) {
        let floor = |seq: &[Seq]| seq.iter().map(|s| s.last.unwrap_or(0)).min().unwrap_or(0);
        let cf = floor(&self.commit_seq).saturating_sub(self.retain);
        if cf > 0 {
            self.commits = self.commits.split_off(&cf);
        }
        let rf = floor(&self.root_seq).saturating_sub(self.retain);
        if rf > 0 {
            self.roots = self.roots.split_off(&rf);
        }
    }

    pub fn n(&self) -> usize {
        self.n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn id(h: u64) -> String {
        format!("0x{:064x}", h * 7 + 1)
    }

    fn feed_ok(c: &mut SafetyChecker, node: usize, from: u64, to: u64) {
        for h in from..=to {
            c.ingest_commit_line(node, &format!("{h} {} {}", id(h), 1000 + h));
            c.ingest_root_line(node, &format!("{h} {} 3", id(h + 1000)));
        }
    }

    #[test]
    fn agreeing_nodes_no_violation() {
        let mut c = SafetyChecker::new(3, false);
        for n in 0..3 {
            feed_ok(&mut c, n, 1, 50);
        }
        assert_eq!(c.safety_violations(), 0);
        assert_eq!(c.stats.cross_checked_commits, 50);
        assert_eq!(c.stats.cross_checked_roots, 50);
        assert_eq!(c.stats.max_committed, 50);
    }

    #[test]
    fn conflicting_commit_detected() {
        let mut c = SafetyChecker::new(2, false);
        feed_ok(&mut c, 0, 1, 10);
        feed_ok(&mut c, 1, 1, 4);
        c.ingest_commit_line(1, &format!("5 {} 1", id(999)));
        assert_eq!(c.safety_violations(), 1);
        assert_eq!(c.violations[0].kind, ViolationKind::ConflictingCommit);
        assert_eq!(c.violations[0].height, 5);
        assert_eq!(c.violations[0].node, 1);
    }

    #[test]
    fn conflict_detected_regardless_of_order_and_case() {
        let mut c = SafetyChecker::new(2, false);
        c.ingest_commit_line(1, "1 0xABCD 5");
        c.ingest_commit_line(0, "1 abcd 5"); // same id, different spelling
        assert_eq!(c.safety_violations(), 0);
        c.ingest_commit_line(0, "2 abce 6");
        c.ingest_commit_line(1, "2 abcf 6");
        assert_eq!(c.by_kind.get(&ViolationKind::ConflictingCommit), Some(&1));
    }

    #[test]
    fn root_mismatch_detected() {
        let mut c = SafetyChecker::new(3, false);
        feed_ok(&mut c, 0, 1, 5);
        feed_ok(&mut c, 1, 1, 5);
        c.ingest_root_line(2, &format!("1 {} 0", id(1001)));
        c.ingest_root_line(2, &format!("2 {} 0", id(4242)));
        assert_eq!(c.safety_violations(), 1);
        assert_eq!(c.violations[0].kind, ViolationKind::RootMismatch);
        assert_eq!(c.violations[0].height, 2);
    }

    #[test]
    fn gaps_detected_and_optional() {
        let mut c = SafetyChecker::new(1, false);
        c.ingest_commit_line(0, &format!("1 {} 1", id(1)));
        c.ingest_commit_line(0, &format!("2 {} 1", id(2)));
        c.ingest_commit_line(0, &format!("5 {} 1", id(5)));
        c.ingest_root_line(0, &format!("2 {} 1", id(2))); // first root must be 1
        assert_eq!(c.by_kind.get(&ViolationKind::CommitGap), Some(&1));
        assert_eq!(c.by_kind.get(&ViolationKind::RootGap), Some(&1));
        // contiguity continues from the new height
        c.ingest_commit_line(0, &format!("6 {} 1", id(6)));
        assert_eq!(c.safety_violations(), 2);

        let mut c = SafetyChecker::new(1, true);
        c.ingest_commit_line(0, &format!("1 {} 1", id(1)));
        c.ingest_commit_line(0, &format!("9 {} 1", id(9)));
        assert_eq!(c.safety_violations(), 0);
    }

    #[test]
    fn relog_after_restart_is_fine_but_self_conflict_is_not() {
        let mut c = SafetyChecker::new(1, false);
        feed_ok(&mut c, 0, 1, 10);
        feed_ok(&mut c, 0, 8, 12); // replayed 8..10 then new 11, 12
        assert_eq!(c.safety_violations(), 0);
        assert_eq!(c.stats.relogged, 6);
        c.ingest_commit_line(0, &format!("3 {} 1", id(77)));
        assert_eq!(c.by_kind.get(&ViolationKind::ConflictingCommit), Some(&1));
    }

    #[test]
    fn divergence_and_malformed() {
        let mut c = SafetyChecker::new(1, false);
        c.ingest_log_line(0, "2026 INFO consensus: committed 5");
        c.ingest_log_line(0, "2026 ERROR consensus: STATE DIVERGENCE at exec height 7");
        assert_eq!(c.by_kind.get(&ViolationKind::StateDivergence), Some(&1));
        c.ingest_commit_line(0, "garbage");
        c.ingest_commit_line(0, "1 nothex 3");
        c.ingest_commit_line(0, "1 abcd");
        assert_eq!(c.stats.malformed_lines, 3);
        assert_eq!(c.safety_violations(), 1);
    }

    #[test]
    fn pruning_keeps_checking_recent_heights() {
        let mut c = SafetyChecker::new(2, false);
        c.retain = 10;
        feed_ok(&mut c, 0, 1, 100);
        feed_ok(&mut c, 1, 1, 100);
        c.prune();
        assert!(c.commits.len() <= 12);
        c.ingest_commit_line(1, &format!("99 {} 1", id(5)));
        assert_eq!(c.safety_violations(), 1);
    }

    #[test]
    fn tails_files_and_handles_torn_lines() {
        let dir = crate::testutil::tempdir("checker");
        let homes: Vec<_> = (0..2).map(|i| dir.join(format!("node{i}"))).collect();
        for h in &homes {
            std::fs::create_dir_all(h).unwrap();
        }
        let mut c = SafetyChecker::with_homes(&homes, &["console.log"], false);
        let app = |p: &std::path::Path, s: &str| {
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p).unwrap();
            f.write_all(s.as_bytes()).unwrap();
        };
        app(&homes[0].join("commits.log"), &format!("1 {} 1\n2 {} 2\n3 {}", id(1), id(2), &id(3)[..10]));
        app(&homes[1].join("commits.log"), &format!("1 {} 1\n", id(1)));
        c.poll();
        assert_eq!(c.node_committed(0), Some(2));
        assert_eq!(c.node_committed(1), Some(1));
        c.on_killed(0); // node0 died mid-line
        assert_eq!(c.stats.torn_lines, 1);
        app(&homes[0].join("commits.log"), &format!("\n3 {} 3\n", id(3)));
        app(&homes[1].join("console.log"), "boom STATE DIVERGENCE height 3\n");
        c.poll();
        assert_eq!(c.node_committed(0), Some(3));
        assert_eq!(c.by_kind.get(&ViolationKind::StateDivergence), Some(&1));
        assert_eq!(c.by_kind.get(&ViolationKind::ConflictingCommit), None);
        assert_eq!(c.stats.malformed_lines, 0);
    }
}
