//! Block log: a zstd-compressed stream of borsh records.
//!
//! Decompressed layout:
//!
//! ```text
//! MAGIC "HLBFTLOG" | u32 LE version
//! u32 LE len | borsh(LogHeader)
//! repeated:  0x01 | u32 LE len | borsh(BlockRecord)
//! end:       0xFF | u32 LE len | borsh(LogFooter)
//! ```
//!
//! Written and read strictly as a stream (multi-GB logs never sit in
//! memory). The writer is single-threaded zstd at a fixed level, so the same
//! record sequence always yields byte-identical files.

use borsh::{BorshDeserialize, BorshSerialize};
use interfaces::*;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

pub const MAGIC: &[u8; 8] = b"HLBFTLOG";
pub const VERSION: u32 = 1;
const TAG_BLOCK: u8 = 0x01;
const TAG_END: u8 = 0xFF;
/// Sanity bound on a single record (a block is bounded by max_block_bytes).
pub const MAX_RECORD_BYTES: u32 = 256 * 1024 * 1024;
pub const ZSTD_LEVEL: i32 = 3;

/// Generator parameters recorded in the header.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, serde::Serialize, serde::Deserialize)]
pub struct GenParams {
    pub seed: u64,
    /// Target number of actions (see README: Σ max(1, order actions) over user txs).
    pub actions: u64,
    pub blocks: u64,
    pub block_ms: u64,
    pub validators: u32,
    pub accounts: u32,
    /// Fraction (parts per million) of user txs made intentionally invalid.
    pub invalid_ppm: u32,
    /// Number of validators submitting skewed oracle prices.
    pub skewed_validators: u32,
    /// Markets by name, in asset order.
    pub markets: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct LogHeader {
    pub genesis_hash: Hash32,
    pub seed: u64,
    pub params: GenParams,
    /// Full genesis so a log is self-contained for replay.
    pub genesis: Genesis,
    /// e.g. "sim 0.1.0".
    pub generator: String,
}

/// One block of the log. `timestamp_ms` is the block time the leader
/// proposes; `proposer` the round-robin leader used during generation.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct BlockRecord {
    pub height: Height,
    pub timestamp_ms: TimestampMs,
    pub proposer: Address,
    pub txs: Vec<Tx>,
}

impl BlockRecord {
    /// Synthetic block id used for offline execution contexts
    /// (blake3 of height, timestamp, proposer and payload hash). The state
    /// machine does not hash block ids into state, so network replay roots
    /// are comparable.
    pub fn synthetic_block_id(&self) -> BlockId {
        let ph = Block::payload_hash(&self.txs);
        hash_borsh(&(self.height, self.timestamp_ms, self.proposer, ph))
    }
    pub fn context(&self) -> BlockContext {
        BlockContext {
            height: self.height,
            timestamp_ms: self.timestamp_ms,
            proposer: self.proposer,
            block_id: self.synthetic_block_id(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize, serde::Serialize, serde::Deserialize)]
pub struct LogFooter {
    pub blocks: u64,
    pub user_txs: u64,
    pub validator_txs: u64,
    /// Σ order_action_count (orders / cancels / modifies) over user txs.
    pub order_actions: u64,
    /// Σ max(1, order_action_count) over user txs.
    pub actions: u64,
    /// blake3 over every borsh(BlockRecord) in order.
    pub records_digest: Hash32,
}

/// Streaming writer. Call [`LogWriter::finish`] to write the footer and
/// flush; dropping without `finish` leaves a truncated (detectably invalid) log.
pub struct LogWriter<W: Write> {
    enc: zstd::Encoder<'static, BufWriter<W>>,
    footer: LogFooter,
    digest: blake3::Hasher,
    buf: Vec<u8>,
    last_height: Height,
}

impl LogWriter<File> {
    pub fn create(path: &Path, header: &LogHeader) -> anyhow::Result<Self> {
        LogWriter::new(File::create(path)?, header)
    }
}

impl<W: Write> LogWriter<W> {
    pub fn new(w: W, header: &LogHeader) -> anyhow::Result<Self> {
        let mut enc = zstd::Encoder::new(BufWriter::with_capacity(1 << 20, w), ZSTD_LEVEL)?;
        enc.include_checksum(true)?;
        enc.write_all(MAGIC)?;
        enc.write_all(&VERSION.to_le_bytes())?;
        let h = borsh::to_vec(header)?;
        enc.write_all(&(h.len() as u32).to_le_bytes())?;
        enc.write_all(&h)?;
        Ok(LogWriter { enc, footer: LogFooter::default(), digest: blake3::Hasher::new(), buf: Vec::new(), last_height: 0 })
    }

    pub fn append(&mut self, rec: &BlockRecord) -> anyhow::Result<()> {
        anyhow::ensure!(rec.height == self.last_height + 1, "non-contiguous height {} after {}", rec.height, self.last_height);
        self.buf.clear();
        rec.serialize(&mut self.buf)?;
        anyhow::ensure!(self.buf.len() as u64 <= MAX_RECORD_BYTES as u64, "record too large");
        self.enc.write_all(&[TAG_BLOCK])?;
        self.enc.write_all(&(self.buf.len() as u32).to_le_bytes())?;
        self.enc.write_all(&self.buf)?;
        self.digest.update(&self.buf);
        self.last_height = rec.height;
        let f = &mut self.footer;
        f.blocks += 1;
        for tx in &rec.txs {
            match tx {
                Tx::User(u) => {
                    f.user_txs += 1;
                    let n = crate::sign::order_action_count_bytes(&u.action_bytes);
                    f.order_actions += n;
                    f.actions += n.max(1);
                }
                Tx::Validator(_) => f.validator_txs += 1,
            }
        }
        Ok(())
    }

    pub fn footer_so_far(&self) -> &LogFooter {
        &self.footer
    }

    pub fn finish(mut self) -> anyhow::Result<LogFooter> {
        self.footer.records_digest = Hash32(*self.digest.finalize().as_bytes());
        let f = borsh::to_vec(&self.footer)?;
        self.enc.write_all(&[TAG_END])?;
        self.enc.write_all(&(f.len() as u32).to_le_bytes())?;
        self.enc.write_all(&f)?;
        let mut bw = self.enc.finish()?;
        bw.flush()?;
        Ok(self.footer)
    }
}

/// Streaming reader: `header()` then iterate blocks; `footer()` is available
/// once iteration returned `None` (a missing footer is an error).
pub struct LogReader<R: Read> {
    dec: zstd::Decoder<'static, BufReader<R>>,
    header: LogHeader,
    footer: Option<LogFooter>,
    buf: Vec<u8>,
    digest: blake3::Hasher,
    last_height: Height,
    done: bool,
}

impl LogReader<File> {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        LogReader::new(File::open(path)?)
    }
}

fn read_u32<R: Read>(r: &mut R) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

impl<R: Read> LogReader<R> {
    pub fn new(r: R) -> anyhow::Result<Self> {
        let mut dec = zstd::Decoder::new(r)?;
        let mut m = [0u8; 8];
        dec.read_exact(&mut m)?;
        anyhow::ensure!(&m == MAGIC, "not an hlbft block log");
        let v = read_u32(&mut dec)?;
        anyhow::ensure!(v == VERSION, "unsupported log version {v}");
        let n = read_u32(&mut dec)?;
        anyhow::ensure!(n <= MAX_RECORD_BYTES, "header too large");
        let mut hb = vec![0u8; n as usize];
        dec.read_exact(&mut hb)?;
        let header = LogHeader::try_from_slice(&hb)?;
        anyhow::ensure!(
            crate::genesis::genesis_hash(&header.genesis) == header.genesis_hash,
            "header genesis hash mismatch"
        );
        Ok(LogReader { dec, header, footer: None, buf: Vec::new(), digest: blake3::Hasher::new(), last_height: 0, done: false })
    }

    pub fn header(&self) -> &LogHeader {
        &self.header
    }

    pub fn footer(&self) -> Option<&LogFooter> {
        self.footer.as_ref()
    }

    /// Next block, `Ok(None)` at a valid end of log.
    pub fn next_block(&mut self) -> anyhow::Result<Option<BlockRecord>> {
        if self.done {
            return Ok(None);
        }
        let mut tag = [0u8; 1];
        self.dec.read_exact(&mut tag).map_err(|e| anyhow::anyhow!("truncated log after height {}: {e}", self.last_height))?;
        let n = read_u32(&mut self.dec)?;
        anyhow::ensure!(n <= MAX_RECORD_BYTES, "record too large ({n} bytes)");
        self.buf.resize(n as usize, 0);
        self.dec.read_exact(&mut self.buf).map_err(|e| anyhow::anyhow!("truncated record: {e}"))?;
        match tag[0] {
            TAG_BLOCK => {
                let rec = BlockRecord::try_from_slice(&self.buf)?;
                anyhow::ensure!(rec.height == self.last_height + 1, "non-contiguous height {}", rec.height);
                self.digest.update(&self.buf);
                self.last_height = rec.height;
                Ok(Some(rec))
            }
            TAG_END => {
                let f = LogFooter::try_from_slice(&self.buf)?;
                let d = Hash32(*self.digest.finalize().as_bytes());
                anyhow::ensure!(f.records_digest == d, "records digest mismatch");
                anyhow::ensure!(f.blocks == self.last_height, "footer block count mismatch");
                self.footer = Some(f);
                self.done = true;
                Ok(None)
            }
            t => anyhow::bail!("bad record tag {t:#x}"),
        }
    }
}

impl<R: Read> Iterator for LogReader<R> {
    type Item = anyhow::Result<BlockRecord>;
    fn next(&mut self) -> Option<Self::Item> {
        self.next_block().transpose()
    }
}
