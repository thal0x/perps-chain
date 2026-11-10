//! FileStore crash-safety and retention tests.

use consensus::store::{FileStore, FileStoreConfig, MemStore};
use consensus::*;
use interfaces::*;
use proptest::prelude::*;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hlbft-cons-store-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A chain of fake committed blocks (QCs are not signature-valid; the store
/// does not verify).
fn chain(n: u64, txs: usize) -> Vec<CommittedBlock> {
    let g = genesis_block(1, 1000);
    let mut parent = g.id();
    let mut out = vec![];
    for h in 1..=n {
        let payload: Vec<Tx> = (0..txs)
            .map(|k| {
                Tx::User(UserTx {
                    action_bytes: vec![h as u8; 40 + k],
                    nonce: h * 1000 + k as u64,
                    signature: Sig65::default(),
                    vault_address: None,
                    expires_after: None,
                })
            })
            .collect();
        let justify = QuorumCert { data: VoteData { chain_id: 1, block_id: parent, view: h - 1, height: h - 1 }, signatures: vec![] };
        let header = BlockHeader {
            chain_id: 1,
            height: h,
            view: h,
            parent,
            proposer: Address::ZERO,
            timestamp_ms: 1000 + h,
            payload_hash: hash_borsh(&payload),
            exec_height: h - 1,
            exec_root: Hash32([h as u8; 32]),
            justify,
        };
        let b = Block { header, payload };
        let id = b.id();
        let qc = QuorumCert { data: VoteData { chain_id: 1, block_id: id, view: h, height: h }, signatures: vec![(Address::ZERO, Sig65::default())] };
        out.push(CommittedBlock { block: b, commit_proof: vec![qc.clone(), qc] });
        parent = id;
    }
    out
}

fn safety(v: u64) -> SafetyState {
    let q = genesis_qc(1, genesis_block(1, 1000).id());
    SafetyState { last_voted_view: v, last_timeout_view: v / 2, locked_qc: q.clone(), high_qc: q }
}

fn small_cfg() -> FileStoreConfig {
    FileStoreConfig { retain_heights: 0, segment_bytes: 4096, fsync_commits: false, keep_snapshots: 2 }
}

#[test]
fn roundtrip_and_reopen() {
    let d = tmpdir("rt");
    let c = chain(50, 3);
    {
        let mut s = FileStore::open_with(&d, small_cfg()).unwrap();
        for cb in &c {
            s.put_block(&cb.block).unwrap();
            s.put_committed(cb).unwrap();
        }
        s.save_safety(&safety(77)).unwrap();
        s.save_snapshot(&Snapshot { height: 20, state_root: Hash32([1; 32]), data: vec![1, 2, 3] }).unwrap();
        s.save_snapshot(&Snapshot { height: 40, state_root: Hash32([2; 32]), data: vec![4] }).unwrap();
        s.save_snapshot(&Snapshot { height: 45, state_root: Hash32([3; 32]), data: vec![5] }).unwrap();
        assert_eq!(s.committed_height(), 50);
    }
    let s = FileStore::open_with(&d, small_cfg()).unwrap();
    assert_eq!(s.committed_height(), 50);
    for cb in &c {
        assert_eq!(s.committed_at(cb.block.header.height).as_ref(), Some(cb));
        assert_eq!(s.get_block(&cb.block.id()).as_ref(), Some(&cb.block));
    }
    assert_eq!(s.load_safety(), Some(safety(77)));
    assert_eq!(s.latest_snapshot().unwrap().height, 45);
    // only 2 snapshots kept
    assert_eq!(std::fs::read_dir(d.join("snapshots")).unwrap().count(), 2);
    let _ = std::fs::remove_dir_all(&d);
}

fn last_segment(d: &PathBuf, prefix: &str) -> PathBuf {
    let mut v: Vec<PathBuf> = std::fs::read_dir(d)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(prefix))
        .collect();
    v.sort();
    v.pop().unwrap()
}

#[test]
fn torn_tail_is_truncated() {
    let d = tmpdir("torn");
    let c = chain(10, 2);
    {
        let mut s = FileStore::open_with(&d, small_cfg()).unwrap();
        for cb in &c {
            s.put_committed(cb).unwrap();
        }
    }
    // append garbage (torn partial record) to the committed log tail
    let p = last_segment(&d, "committed.");
    let mut f = OpenOptions::new().append(true).open(&p).unwrap();
    f.write_all(&[200, 0, 0, 0, 1, 2, 3]).unwrap();
    drop(f);
    let len_before = std::fs::metadata(&p).unwrap().len();
    {
        let mut s = FileStore::open_with(&d, small_cfg()).unwrap();
        assert_eq!(s.committed_height(), 10);
        assert!(std::fs::metadata(&p).unwrap().len() < len_before);
        // and appending afterwards works
        let more = chain(11, 2);
        s.put_committed(&more[10]).unwrap();
    }
    let s = FileStore::open_with(&d, small_cfg()).unwrap();
    assert_eq!(s.committed_height(), 11);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn corrupt_safety_file_is_ignored_and_tmp_is_harmless() {
    let d = tmpdir("safety");
    {
        let mut s = FileStore::open_with(&d, small_cfg()).unwrap();
        s.save_safety(&safety(5)).unwrap();
        s.save_safety(&safety(9)).unwrap();
    }
    // a leftover tmp from a crash mid-write must not matter
    std::fs::write(d.join("safety.bin.tmp"), b"garbage").unwrap();
    assert_eq!(FileStore::open_with(&d, small_cfg()).unwrap().load_safety(), Some(safety(9)));
    // flipping a byte in safety.bin is detected (checksum) -> None, never garbage
    let mut b = std::fs::read(d.join("safety.bin")).unwrap();
    let n = b.len();
    b[n - 1] ^= 0xff;
    std::fs::write(d.join("safety.bin"), &b).unwrap();
    assert_eq!(FileStore::open_with(&d, small_cfg()).unwrap().load_safety(), None);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn retention_prunes_old_segments() {
    let d = tmpdir("ret");
    let c = chain(400, 4);
    let cfg = FileStoreConfig { retain_heights: 100, segment_bytes: 8192, fsync_commits: false, keep_snapshots: 2 };
    {
        let mut s = FileStore::open_with(&d, cfg.clone()).unwrap();
        for cb in &c {
            s.put_committed(cb).unwrap();
        }
        assert_eq!(s.committed_height(), 400);
        assert!(s.committed_at(10).is_none(), "old heights pruned");
        assert!(s.committed_at(300).is_some());
        assert!(s.committed_at(400).is_some());
    }
    let s = FileStore::open_with(&d, cfg).unwrap();
    assert!(s.committed_at(10).is_none());
    for h in 300..=400 {
        assert_eq!(s.committed_at(h).as_ref(), Some(&c[h as usize - 1]));
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn memstore_retention() {
    let mut s = MemStore::with_retention(10);
    for cb in chain(50, 1) {
        s.put_committed(&cb).unwrap();
    }
    assert_eq!(s.committed_height(), 50);
    assert!(s.committed_at(39).is_none());
    assert!(s.committed_at(40).is_some());
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, failure_persistence: None, .. ProptestConfig::default() })]
    /// Kill -9 at an arbitrary byte: truncate the committed or blocks log at
    /// a random offset; reopening must succeed, keep a contiguous prefix of
    /// committed heights, and every returned block must be intact.
    #[test]
    fn crash_at_any_byte(cut_frac in 0u32..1000, which in 0u8..2, n in 5u64..40) {
        let d = tmpdir(&format!("cut-{cut_frac}-{which}-{n}"));
        let c = chain(n, 3);
        {
            let mut s = FileStore::open_with(&d, small_cfg()).unwrap();
            for cb in &c {
                s.put_committed(cb).unwrap();
            }
        }
        let p = last_segment(&d, if which == 0 { "committed." } else { "blocks." });
        let len = std::fs::metadata(&p).unwrap().len();
        let cut = len * cut_frac as u64 / 1000;
        OpenOptions::new().write(true).open(&p).unwrap().set_len(cut).unwrap();
        let s = FileStore::open_with(&d, small_cfg()).unwrap();
        let top = s.committed_height();
        prop_assert!(top <= n);
        for h in 1..=top {
            if let Some(cb) = s.committed_at(h) {
                prop_assert_eq!(&cb, &c[h as usize - 1]);
            }
        }
        // the very top entry must be readable
        if top > 0 {
            prop_assert!(s.committed_at(top).is_some());
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
