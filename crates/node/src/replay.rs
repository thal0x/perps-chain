//! `--replay-log` (D8): the leader proposes the logged block for each height
//! and voters refuse anything else (consensus `proposal_override`).
//!
//! The log (`sim::log`, zstd stream of borsh records) is read as a stream:
//! a window of recent records is kept in memory and the reader runs ahead on
//! demand. A request below the window (e.g. after a restart in replay mode)
//! reopens the log and scans forward. After the last logged height two empty
//! padding blocks (timestamps +1 ms each) are returned so the last logged
//! block can commit under the 2-chain rule (D20).
//!
//! The genesis comes from the log header; [`ReplaySource::open`] refuses a
//! mismatching genesis supplied by the operator.

use anyhow::{bail, ensure, Context, Result};
use interfaces::*;
use sim::log::LogReader;
use std::collections::VecDeque;
use std::fs::File;
use std::path::{Path, PathBuf};

/// Records kept behind the highest height read.
const KEEP_BEHIND: u64 = 64;

pub struct ReplaySource {
    path: Option<PathBuf>,
    reader: Option<LogReader<File>>,
    /// Contiguous window of records, ascending.
    window: VecDeque<(Height, TimestampMs, Vec<Tx>)>,
    /// Highest height read from the reader so far.
    read_to: Height,
    last: Height,
    last_ts: TimestampMs,
    genesis: Genesis,
    /// Set when the log failed validation (footer digest, count, truncation).
    pub error: Option<String>,
    /// Exit the process on a log error (the node); tests keep it false.
    pub exit_on_error: bool,
}

/// Read only the log header's genesis (cheap: the header is the first record).
pub fn log_genesis(path: &Path) -> Result<Genesis> {
    let r = LogReader::open(path).with_context(|| format!("open replay log {}", path.display()))?;
    Ok(r.header().genesis.clone())
}

impl ReplaySource {
    /// Open `path`. `genesis`, if given (an operator-supplied genesis.json),
    /// must equal the genesis embedded in the log header.
    pub fn open(path: &Path, genesis: Option<&Genesis>) -> Result<ReplaySource> {
        let reader = LogReader::open(path).with_context(|| format!("open replay log {}", path.display()))?;
        let h = reader.header().clone();
        if let Some(g) = genesis {
            if g != &h.genesis {
                bail!(
                    "--replay-log {}: genesis mismatch: the supplied genesis (hash {}) differs from the log header's (hash {}); \
                     refusing to replay",
                    path.display(),
                    hash_borsh(g),
                    h.genesis_hash
                );
            }
        }
        let last = h.params.blocks;
        ensure!(last > 0, "replay log {} has no blocks", path.display());
        Ok(ReplaySource {
            path: Some(path.to_path_buf()),
            reader: Some(reader),
            window: VecDeque::new(),
            read_to: 0,
            last,
            last_ts: 0,
            genesis: h.genesis,
            error: None,
            exit_on_error: false,
        })
    }

    /// Build from in-memory records (tests).
    pub fn from_blocks(genesis: Genesis, blocks: Vec<(Height, TimestampMs, Vec<Tx>)>) -> ReplaySource {
        let last = blocks.iter().map(|b| b.0).max().unwrap_or(0);
        let last_ts = blocks.iter().find(|b| b.0 == last).map(|b| b.1).unwrap_or(0);
        let mut window: Vec<_> = blocks;
        window.sort_by_key(|b| b.0);
        ReplaySource {
            path: None,
            reader: None,
            read_to: last,
            window: window.into(),
            last,
            last_ts,
            genesis,
            error: None,
            exit_on_error: false,
        }
    }

    pub fn genesis(&self) -> &Genesis {
        &self.genesis
    }

    pub fn last_height(&self) -> Height {
        self.last
    }

    fn reopen(&mut self) -> Result<()> {
        let p = self.path.clone().context("in-memory replay source cannot rewind")?;
        tracing::warn!(path = %p.display(), "replay: rewinding the block log");
        self.reader = Some(LogReader::open(&p)?);
        self.window.clear();
        self.read_to = 0;
        Ok(())
    }

    /// Read forward until `h` is in the window (or the log ends).
    fn read_until(&mut self, h: Height) -> Result<()> {
        while self.read_to < h {
            let Some(r) = self.reader.as_mut() else { return Ok(()) };
            match r.next_block()? {
                Some(rec) => {
                    ensure!(rec.height == self.read_to + 1, "replay log: height {} after {}", rec.height, self.read_to);
                    ensure!(rec.height <= self.last, "replay log has more blocks than its header ({})", self.last);
                    self.read_to = rec.height;
                    if rec.height == self.last {
                        self.last_ts = rec.timestamp_ms;
                    }
                    self.window.push_back((rec.height, rec.timestamp_ms, rec.txs));
                    while self.window.front().is_some_and(|f| f.0 + KEEP_BEHIND < self.read_to) {
                        self.window.pop_front();
                    }
                    if rec.height == self.last {
                        // consume + validate the footer (digest, block count)
                        ensure!(r.next_block()?.is_none(), "replay log continues past its header's block count");
                        let f = r.footer().context("replay log has no footer")?;
                        ensure!(f.blocks == self.last, "footer says {} blocks, header {}", f.blocks, self.last);
                    }
                }
                None => bail!("replay log ended at height {} before the header's {} blocks", self.read_to, self.last),
            }
        }
        Ok(())
    }

    fn lookup(&self, h: Height) -> Option<(TimestampMs, Vec<Tx>)> {
        let first = self.window.front()?.0;
        if h < first {
            return None;
        }
        self.window.get((h - first) as usize).filter(|b| b.0 == h).map(|b| (b.1, b.2.clone()))
    }

    pub fn get(&mut self, h: Height) -> Option<(TimestampMs, Vec<Tx>)> {
        if h == 0 {
            return None;
        }
        if h > self.last {
            if h <= self.last + 2 {
                if self.last_ts == 0 {
                    if let Err(e) = self.read_until(self.last) {
                        self.fail(e);
                    }
                }
                return Some((self.last_ts + (h - self.last), Vec::new()));
            }
            return None;
        }
        if let Some(b) = self.lookup(h) {
            return Some(b);
        }
        if self.window.front().is_some_and(|f| h < f.0) || (self.window.is_empty() && self.read_to >= h) {
            if let Err(e) = self.reopen() {
                self.fail(e);
                return None;
            }
        }
        if let Err(e) = self.read_until(h) {
            self.fail(e);
            return None;
        }
        self.lookup(h)
    }

    /// A corrupt / truncated log cannot be replayed faithfully: proposing
    /// anything else would silently diverge from the log, so the node stops.
    fn fail(&mut self, e: anyhow::Error) {
        tracing::error!("replay log error: {e:#}");
        eprintln!("hlnode: fatal: replay log error: {e:#}");
        self.error = Some(format!("{e:#}"));
        if self.exit_on_error {
            std::process::exit(4);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim::gen::{generate, GenConfig};

    fn small_log(dir: &Path) -> (PathBuf, Genesis) {
        let mut cfg = GenConfig::small(11);
        cfg.actions = 600;
        cfg.sim_ms = 40_000;
        let p = dir.join("t.log.zst");
        let s = generate(&cfg, &sim::null_factory(), File::create(&p).unwrap(), |_, _| {}).unwrap();
        assert_eq!(s.footer.blocks, 40);
        (p, sim::gen::keys_for(&cfg).0)
    }

    #[test]
    fn streams_rewinds_pads_and_checks_genesis() {
        let dir = std::env::temp_dir().join(format!("hlnode-replay-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (p, g) = small_log(&dir);
        // reference: all records
        let mut all = Vec::new();
        let mut r = LogReader::open(&p).unwrap();
        while let Some(b) = r.next_block().unwrap() {
            all.push(b);
        }
        let mut src = ReplaySource::open(&p, Some(&g)).unwrap();
        assert_eq!(src.genesis(), &g);
        assert_eq!(src.last_height(), 40);
        assert_eq!(src.get(0), None);
        for h in 1..=40u64 {
            let b = src.get(h).unwrap();
            assert_eq!(b.0, all[h as usize - 1].timestamp_ms);
            assert_eq!(b.1, all[h as usize - 1].txs);
            // repeated + previous-height lookups (voters, retries)
            assert_eq!(src.get(h), Some(b.clone()));
            if h > 1 {
                assert_eq!(src.get(h - 1).unwrap().0, all[h as usize - 2].timestamp_ms);
            }
        }
        let lt = all[39].timestamp_ms;
        assert_eq!(src.get(41), Some((lt + 1, vec![])));
        assert_eq!(src.get(42), Some((lt + 2, vec![])));
        assert_eq!(src.get(43), None);
        // rewind (restart-like lookup below the window works because window keeps 64; force it)
        assert_eq!(src.get(1).unwrap().1, all[0].txs);
        assert!(src.error.is_none());

        // padding without having read the log yet
        let mut fresh = ReplaySource::open(&p, None).unwrap();
        assert_eq!(fresh.get(41), Some((lt + 1, vec![])));

        // mismatching genesis is refused
        let mut bad = g.clone();
        bad.chain_id += 1;
        let e = ReplaySource::open(&p, Some(&bad)).err().unwrap().to_string();
        assert!(e.contains("genesis mismatch"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewinds_below_window() {
        let dir = std::env::temp_dir().join(format!("hlnode-replay-rw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = GenConfig::small(12);
        cfg.actions = 2000;
        cfg.sim_ms = 150_000;
        let p = dir.join("t.log.zst");
        generate(&cfg, &sim::null_factory(), File::create(&p).unwrap(), |_, _| {}).unwrap();
        let mut src = ReplaySource::open(&p, None).unwrap();
        let t5 = src.get(5).unwrap();
        let _ = src.get(150).unwrap();
        assert_eq!(src.get(5), Some(t5));
        assert!(src.error.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
