//! Post-run QC verification of every node's consensus FileStore
//! (`<home>/data`): the retained committed run must link by parent id and
//! carry valid commit proofs (`consensus::CertCtx::verify_committed_run`),
//! and each block id must equal what the node wrote to `commits.log`.
//!
//! Runs only after the nodes are stopped (opening a FileStore truncates torn
//! tails). Nodes without a `data/` dir (e.g. the fake node) are skipped.

use interfaces::{BlockStore, CommittedBlock, Genesis, Height};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug, Default, Serialize)]
pub struct QcReport {
    pub nodes_checked: usize,
    pub nodes_skipped: usize,
    pub blocks_verified: u64,
    pub failures: Vec<String>,
}

/// Blocks above the highest direct commit proof cannot be proven; allow
/// this many unproven blocks at the tip.
const UNPROVEN_TIP_SLACK: usize = 4;

/// Read `commits.log` into height -> block id (lowercase hex, no 0x).
fn commit_ids(home: &Path) -> BTreeMap<Height, String> {
    let mut m = BTreeMap::new();
    if let Ok(s) = std::fs::read_to_string(home.join("commits.log")) {
        for l in s.lines() {
            if let Some((h, id)) = crate::checker::parse_line(l) {
                m.insert(h, id);
            }
        }
    }
    m
}

pub fn verify_homes(genesis_path: &Path, homes: &[impl AsRef<Path>], max_blocks: u64) -> QcReport {
    let mut rep = QcReport::default();
    let genesis: Genesis = match std::fs::read(genesis_path).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
        Some(g) => g,
        None => {
            rep.nodes_skipped = homes.len();
            return rep;
        }
    };
    let genesis_id = consensus::genesis_block(genesis.chain_id, genesis.genesis_time_ms).header.id();
    let ctx = consensus::CertCtx { chain_id: genesis.chain_id, genesis_id, validators: &genesis.validators };
    let verify = |d: &[u8; 32], s: &interfaces::Sig65, a: &interfaces::Address| hlwire::verify(d, s, a);
    for (i, home) in homes.iter().enumerate() {
        let home = home.as_ref();
        let data = home.join("data");
        if !data.is_dir() {
            rep.nodes_skipped += 1;
            continue;
        }
        let store = match consensus::FileStore::open_with(&data, consensus::FileStoreConfig::default()) {
            Ok(s) => s,
            Err(e) => {
                rep.failures.push(format!("node{i}: cannot open FileStore: {e}"));
                continue;
            }
        };
        rep.nodes_checked += 1;
        let top = store.committed_height();
        if top == 0 {
            continue;
        }
        // retained window: walk down until a height is missing
        let floor = top.saturating_sub(max_blocks).max(1);
        let mut start = top;
        while start > floor && store.committed_at(start - 1).is_some() {
            start -= 1;
        }
        let blocks: Vec<CommittedBlock> = (start..=top).map_while(|h| store.committed_at(h)).collect();
        let anchor = if start == 1 {
            Some((0, genesis_id))
        } else {
            store.committed_at(start - 1).map(|b| (start - 1, b.block.header.id()))
        };
        let proven = ctx.verify_committed_run(&blocks, anchor, &verify);
        rep.blocks_verified += proven as u64;
        if proven + UNPROVEN_TIP_SLACK < blocks.len() {
            let h = start + proven as u64;
            rep.failures.push(format!("node{i}: committed run {start}..={top}: only {proven} blocks proven; first failure at height {h}"));
        }
        let ids = commit_ids(home);
        for cb in &blocks {
            let h = cb.block.header.height;
            if let Some(logged) = ids.get(&h) {
                let id = hex::encode(cb.block.header.id().0);
                if *logged != id {
                    rep.failures.push(format!("node{i}: height {h}: FileStore block {id} != commits.log {logged}"));
                    break;
                }
            }
        }
    }
    rep
}
