//! Block stores: [`MemStore`] (in-memory, cloneable; tests/simulation) and
//! [`FileStore`] (crash-safe, append-only files).
//!
//! ## FileStore layout (`dir/`)
//! ```text
//! blocks.<seq:08>.log      records: borsh(Block)            (segmented)
//! committed.<seq:08>.log   records: borsh(CommittedRec)     (segmented)
//! safety.bin               checksum || borsh(SafetyState)   (tmp + fsync + rename)
//! snapshots/<h:020>.snap   checksum || borsh(Snapshot)      (tmp + fsync + rename)
//! ```
//! Log record framing: `len: u32 LE | checksum: [u8; 8] | body[len]` with
//! `checksum = blake3(body)[..8]`. On open every segment is scanned; the first
//! incomplete or corrupt record and everything after it in that segment is
//! truncated (a torn tail after `kill -9`). A committed record refers to its
//! block by id; the block is appended (and, with `fsync_commits`, fsynced)
//! before the committed record, so a committed record never dangles after a
//! crash. If it does anyway (disk corruption), committed records from that
//! height upwards are dropped and re-synced from peers.
//!
//! Retention: committed records and blocks more than `retain_heights` below
//! the committed height are deleted segment-wise.

use borsh::{BorshDeserialize, BorshSerialize};
use interfaces::*;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

// ------------------------------------------------------------------ memory --

/// In-memory store. `Clone` it to model "what was on disk" across a
/// simulated crash.
#[derive(Clone, Default)]
pub struct MemStore {
    blocks: BTreeMap<BlockId, Block>,
    committed: BTreeMap<Height, CommittedBlock>,
    safety: Option<SafetyState>,
    snapshots: BTreeMap<Height, Snapshot>,
    /// Committed blocks older than this many heights are pruned (0 = keep all).
    pub retain_heights: u64,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_retention(retain_heights: u64) -> Self {
        Self { retain_heights, ..Self::default() }
    }
    pub fn lowest_committed(&self) -> Option<Height> {
        self.committed.keys().next().copied()
    }
}

impl BlockStore for MemStore {
    fn put_block(&mut self, b: &Block) -> io::Result<()> {
        self.blocks.entry(b.id()).or_insert_with(|| b.clone());
        Ok(())
    }
    fn get_block(&self, id: &BlockId) -> Option<Block> {
        self.blocks.get(id).cloned()
    }
    fn put_committed(&mut self, c: &CommittedBlock) -> io::Result<()> {
        let h = c.block.header.height;
        self.committed.insert(h, c.clone());
        if self.retain_heights > 0 && h > self.retain_heights {
            let cut = h - self.retain_heights;
            let keep = self.committed.split_off(&cut);
            let dropped = std::mem::replace(&mut self.committed, keep);
            for (_, cb) in dropped {
                self.blocks.remove(&cb.block.id());
            }
            self.blocks.retain(|_, b| b.header.height >= cut);
        }
        Ok(())
    }
    fn committed_at(&self, h: Height) -> Option<CommittedBlock> {
        self.committed.get(&h).cloned()
    }
    fn committed_height(&self) -> Height {
        self.committed.keys().next_back().copied().unwrap_or(0)
    }
    fn save_safety(&mut self, s: &SafetyState) -> io::Result<()> {
        self.safety = Some(s.clone());
        Ok(())
    }
    fn load_safety(&self) -> Option<SafetyState> {
        self.safety.clone()
    }
    fn save_snapshot(&mut self, s: &Snapshot) -> io::Result<()> {
        self.snapshots.insert(s.height, s.clone());
        while self.snapshots.len() > 2 {
            let k = *self.snapshots.keys().next().unwrap();
            self.snapshots.remove(&k);
        }
        Ok(())
    }
    fn latest_snapshot(&self) -> Option<Snapshot> {
        self.snapshots.values().next_back().cloned()
    }
}

// -------------------------------------------------------------------- file --

#[derive(Clone, Debug)]
pub struct FileStoreConfig {
    /// Keep committed blocks this many heights below the tip (0 = forever).
    pub retain_heights: u64,
    /// Rotate a log segment when it exceeds this many bytes.
    pub segment_bytes: u64,
    /// fsync block + committed logs on every `put_committed`.
    pub fsync_commits: bool,
    /// Number of snapshots kept.
    pub keep_snapshots: usize,
}

impl Default for FileStoreConfig {
    fn default() -> Self {
        Self { retain_heights: 10_000, segment_bytes: 256 << 20, fsync_commits: true, keep_snapshots: 2 }
    }
}

#[derive(BorshSerialize, BorshDeserialize)]
struct CommittedRec {
    height: Height,
    block_id: BlockId,
    proof: Vec<QuorumCert>,
}

#[derive(Clone, Copy, Debug)]
struct Loc {
    seg: u64,
    off: u64,
    len: u32,
}

struct Segment {
    seq: u64,
    file: File,
    len: u64,
    /// Max height of any record in the segment.
    max_height: Height,
}

struct Log {
    dir: PathBuf,
    prefix: &'static str,
    segs: BTreeMap<u64, Segment>,
}

const HDR: usize = 12;

fn checksum(body: &[u8]) -> [u8; 8] {
    let h = blake3::hash(body);
    let mut c = [0u8; 8];
    c.copy_from_slice(&h.as_bytes()[..8]);
    c
}

impl Log {
    fn path(&self, seq: u64) -> PathBuf {
        self.dir.join(format!("{}.{:08}.log", self.prefix, seq))
    }

    /// Open all segments, scanning records; calls `f(seq, off, len, body)`
    /// for every intact record. Torn/corrupt tails are truncated.
    fn open(dir: &Path, prefix: &'static str, mut f: impl FnMut(u64, u64, u32, &[u8]) -> Height) -> io::Result<Log> {
        let mut seqs = vec![];
        for e in fs::read_dir(dir)? {
            let name = e?.file_name().to_string_lossy().to_string();
            if let Some(rest) = name.strip_prefix(&format!("{prefix}.")) {
                if let Some(num) = rest.strip_suffix(".log") {
                    if let Ok(n) = num.parse::<u64>() {
                        seqs.push(n);
                    }
                }
            }
        }
        seqs.sort_unstable();
        let mut log = Log { dir: dir.to_path_buf(), prefix, segs: BTreeMap::new() };
        for seq in seqs {
            let path = log.path(seq);
            let mut file = OpenOptions::new().read(true).append(true).open(&path)?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)?;
            let mut off = 0usize;
            let mut max_height = 0;
            while off + HDR <= buf.len() {
                let len = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
                let end = off + HDR + len;
                if end > buf.len() {
                    break;
                }
                let body = &buf[off + HDR..end];
                if checksum(body) != buf[off + 4..off + HDR] {
                    break;
                }
                let h = f(seq, (off + HDR) as u64, len as u32, body);
                max_height = max_height.max(h);
                off = end;
            }
            if off != buf.len() {
                tracing::warn!(segment = %path.display(), valid = off, total = buf.len(), "truncating torn log tail");
                file.set_len(off as u64)?;
                file.sync_all()?;
            }
            log.segs.insert(seq, Segment { seq, file, len: off as u64, max_height });
        }
        Ok(log)
    }

    fn append(&mut self, body: &[u8], height: Height, segment_bytes: u64) -> io::Result<Loc> {
        let need_new = match self.segs.values().next_back() {
            None => true,
            Some(s) => s.len > 0 && s.len + body.len() as u64 > segment_bytes,
        };
        if need_new {
            let seq = self.segs.keys().next_back().map(|s| s + 1).unwrap_or(0);
            let file = OpenOptions::new().read(true).append(true).create(true).open(self.path(seq))?;
            self.segs.insert(seq, Segment { seq, file, len: 0, max_height: 0 });
        }
        let seg = self.segs.values_mut().next_back().unwrap();
        let mut rec = Vec::with_capacity(HDR + body.len());
        rec.extend_from_slice(&(body.len() as u32).to_le_bytes());
        rec.extend_from_slice(&checksum(body));
        rec.extend_from_slice(body);
        seg.file.write_all(&rec)?;
        let loc = Loc { seg: seg.seq, off: seg.len + HDR as u64, len: body.len() as u32 };
        seg.len += rec.len() as u64;
        seg.max_height = seg.max_height.max(height);
        Ok(loc)
    }

    fn read(&self, loc: Loc) -> Option<Vec<u8>> {
        let seg = self.segs.get(&loc.seg)?;
        let mut buf = vec![0u8; loc.len as usize];
        seg.file.read_exact_at(&mut buf, loc.off).ok()?;
        Some(buf)
    }

    fn sync_tail(&self) -> io::Result<()> {
        if let Some(s) = self.segs.values().next_back() {
            s.file.sync_data()?;
        }
        Ok(())
    }

    /// Delete whole segments (never the active one) whose max height < cut.
    fn prune(&mut self, cut: Height) -> Vec<u64> {
        let active = self.segs.keys().next_back().copied();
        let doomed: Vec<u64> = self.segs.values().filter(|s| Some(s.seq) != active && s.max_height < cut).map(|s| s.seq).collect();
        for seq in &doomed {
            self.segs.remove(seq);
            let _ = fs::remove_file(self.path(*seq));
        }
        doomed
    }
}

/// Crash-safe file-backed [`BlockStore`].
pub struct FileStore {
    dir: PathBuf,
    cfg: FileStoreConfig,
    blocks: Log,
    committed: Log,
    block_index: HashMap<BlockId, (Loc, Height)>,
    committed_index: BTreeMap<Height, Loc>,
    safety: Option<SafetyState>,
}

fn write_atomic(dir: &Path, name: &str, body: &[u8]) -> io::Result<()> {
    let tmp = dir.join(format!("{name}.tmp"));
    let dst = dir.join(name);
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&checksum(body))?;
        f.write_all(body)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &dst)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn read_checked(path: &Path) -> Option<Vec<u8>> {
    let buf = fs::read(path).ok()?;
    if buf.len() < 8 || checksum(&buf[8..]) != buf[..8] {
        return None;
    }
    Some(buf[8..].to_vec())
}

impl FileStore {
    pub fn open(dir: impl AsRef<Path>) -> io::Result<FileStore> {
        Self::open_with(dir, FileStoreConfig::default())
    }

    pub fn open_with(dir: impl AsRef<Path>, cfg: FileStoreConfig) -> io::Result<FileStore> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(dir.join("snapshots"))?;
        let mut block_index = HashMap::new();
        let blocks = Log::open(&dir, "blocks", |seq, off, len, body| match Block::try_from_slice(body) {
            Ok(b) => {
                block_index.insert(b.id(), (Loc { seg: seq, off, len }, b.header.height));
                b.header.height
            }
            Err(_) => 0,
        })?;
        let mut committed_index = BTreeMap::new();
        let mut recs: BTreeMap<Height, BlockId> = BTreeMap::new();
        let committed = Log::open(&dir, "committed", |seq, off, len, body| match CommittedRec::try_from_slice(body) {
            Ok(r) => {
                // same semantics as put_committed: a record supersedes any
                // entry at or above its height
                committed_index.split_off(&r.height);
                recs.split_off(&r.height);
                committed_index.insert(r.height, Loc { seg: seq, off, len });
                recs.insert(r.height, r.block_id);
                r.height
            }
            Err(_) => 0,
        })?;
        // drop committed records whose block is missing (and everything above)
        if let Some((bad, _)) = recs.iter().find(|(_, id)| !block_index.contains_key(*id)) {
            tracing::warn!(height = bad, "committed record without block; dropping committed index from here");
            committed_index.split_off(bad);
        }
        let safety = read_checked(&dir.join("safety.bin")).and_then(|b| SafetyState::try_from_slice(&b).ok());
        // clean temp files
        for e in fs::read_dir(dir.join("snapshots"))?.flatten() {
            if e.file_name().to_string_lossy().ends_with(".tmp") {
                let _ = fs::remove_file(e.path());
            }
        }
        Ok(FileStore { dir, cfg, blocks, committed, block_index, committed_index, safety })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn snapshot_heights(&self) -> Vec<Height> {
        let mut v: Vec<Height> = fs::read_dir(self.dir.join("snapshots"))
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".snap").and_then(|s| s.parse().ok()))
                    .collect()
            })
            .unwrap_or_default();
        v.sort_unstable();
        v
    }

    fn prune(&mut self) {
        if self.cfg.retain_heights == 0 {
            return;
        }
        let top = self.committed_height();
        if top <= self.cfg.retain_heights {
            return;
        }
        let cut = top - self.cfg.retain_heights;
        let dropped = self.committed.prune(cut);
        if !dropped.is_empty() {
            self.committed_index.retain(|_, l| !dropped.contains(&l.seg));
        }
        let dropped = self.blocks.prune(cut);
        if !dropped.is_empty() {
            self.block_index.retain(|_, (l, _)| !dropped.contains(&l.seg));
        }
    }
}

impl BlockStore for FileStore {
    fn put_block(&mut self, b: &Block) -> io::Result<()> {
        let id = b.id();
        if self.block_index.contains_key(&id) {
            return Ok(());
        }
        let body = borsh::to_vec(b)?;
        let loc = self.blocks.append(&body, b.header.height, self.cfg.segment_bytes)?;
        self.block_index.insert(id, (loc, b.header.height));
        Ok(())
    }

    fn get_block(&self, id: &BlockId) -> Option<Block> {
        let (loc, _) = self.block_index.get(id)?;
        Block::try_from_slice(&self.blocks.read(*loc)?).ok()
    }

    fn put_committed(&mut self, c: &CommittedBlock) -> io::Result<()> {
        self.put_block(&c.block)?;
        let rec = CommittedRec { height: c.block.header.height, block_id: c.block.id(), proof: c.commit_proof.clone() };
        let body = borsh::to_vec(&rec)?;
        if self.cfg.fsync_commits {
            self.blocks.sync_tail()?;
        }
        let loc = self.committed.append(&body, rec.height, self.cfg.segment_bytes)?;
        if self.cfg.fsync_commits {
            self.committed.sync_tail()?;
        }
        // a lower height re-commit (after snapshot) supersedes higher entries
        self.committed_index.split_off(&rec.height);
        self.committed_index.insert(rec.height, loc);
        self.prune();
        Ok(())
    }

    fn committed_at(&self, h: Height) -> Option<CommittedBlock> {
        let loc = self.committed_index.get(&h)?;
        let rec = CommittedRec::try_from_slice(&self.committed.read(*loc)?).ok()?;
        let block = self.get_block(&rec.block_id)?;
        Some(CommittedBlock { block, commit_proof: rec.proof })
    }

    fn committed_height(&self) -> Height {
        self.committed_index.keys().next_back().copied().unwrap_or(0)
    }

    fn save_safety(&mut self, s: &SafetyState) -> io::Result<()> {
        write_atomic(&self.dir, "safety.bin", &borsh::to_vec(s)?)?;
        self.safety = Some(s.clone());
        Ok(())
    }

    fn load_safety(&self) -> Option<SafetyState> {
        self.safety.clone()
    }

    fn save_snapshot(&mut self, s: &Snapshot) -> io::Result<()> {
        let sdir = self.dir.join("snapshots");
        write_atomic(&sdir, &format!("{:020}.snap", s.height), &borsh::to_vec(s)?)?;
        let hs = self.snapshot_heights();
        if hs.len() > self.cfg.keep_snapshots {
            for h in &hs[..hs.len() - self.cfg.keep_snapshots] {
                let _ = fs::remove_file(sdir.join(format!("{h:020}.snap")));
            }
        }
        Ok(())
    }

    fn latest_snapshot(&self) -> Option<Snapshot> {
        let sdir = self.dir.join("snapshots");
        for h in self.snapshot_heights().into_iter().rev() {
            if let Some(b) = read_checked(&sdir.join(format!("{h:020}.snap"))) {
                if let Ok(s) = Snapshot::try_from_slice(&b) {
                    return Some(s);
                }
            }
        }
        None
    }
}
