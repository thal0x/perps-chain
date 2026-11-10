//! `--replay-log` (D8): the leader proposes the logged block for each height.
//!
//! TODO(sim): the block-log reader lives in `sim::log` (crate `sim`), which
//! has not landed on main yet. Once it does, `open` streams
//! `sim::log::LogReader`, checks `header.genesis == genesis`, and `get`
//! returns `(record.timestamp_ms, record.txs)`; after the last logged height
//! it returns two empty padding blocks (timestamps +1 ms each) so the last
//! logged block can commit under the 2-chain rule.

use anyhow::{bail, Result};
use interfaces::*;
use std::collections::BTreeMap;
use std::path::Path;

pub struct ReplaySource {
    blocks: BTreeMap<Height, (TimestampMs, Vec<Tx>)>,
    last: Height,
}

impl ReplaySource {
    pub fn open(path: &Path, _genesis: &Genesis) -> Result<ReplaySource> {
        bail!("--replay-log {}: block-log reader (sim crate) not available yet", path.display())
    }

    /// Build from in-memory records (tests).
    pub fn from_blocks(blocks: Vec<(Height, TimestampMs, Vec<Tx>)>) -> ReplaySource {
        let last = blocks.iter().map(|b| b.0).max().unwrap_or(0);
        ReplaySource { blocks: blocks.into_iter().map(|(h, t, p)| (h, (t, p))).collect(), last }
    }

    pub fn last_height(&self) -> Height {
        self.last
    }

    pub fn get(&mut self, h: Height) -> Option<(TimestampMs, Vec<Tx>)> {
        if let Some(b) = self.blocks.get(&h) {
            // keep memory bounded: everything below h - 64 is no longer needed
            let r = Some(b.clone());
            if h > 64 {
                let keep = self.blocks.split_off(&(h - 64));
                self.blocks = keep;
            }
            return r;
        }
        if h > self.last && h <= self.last + 2 {
            let base = self.blocks.get(&self.last).map(|b| b.0).unwrap_or(0);
            return Some((base + (h - self.last), Vec::new()));
        }
        None
    }
}
