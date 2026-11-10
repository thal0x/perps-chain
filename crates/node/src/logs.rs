//! `commits.log` / `roots.log` (append-only text, one line per height).
//!
//! On open, a torn last line (no trailing newline, from a kill -9 mid-write)
//! is truncated, and the existing entries are loaded so that heights
//! re-emitted after a restart are verified instead of appended twice.

use interfaces::*;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Line-oriented append-only log keyed by height (first column).
pub struct HeightLog {
    file: File,
    /// Last height present in the file.
    pub last: Height,
    /// Second column of every line, by height.
    pub entries: BTreeMap<Height, String>,
}

impl HeightLog {
    pub fn open(path: &Path) -> io::Result<HeightLog> {
        let mut entries = BTreeMap::new();
        let mut last = 0;
        if path.exists() {
            let data = std::fs::read(path)?;
            let good = match data.iter().rposition(|&b| b == b'\n') {
                Some(i) => i + 1,
                None => 0,
            };
            if good != data.len() {
                let f = OpenOptions::new().write(true).open(path)?;
                f.set_len(good as u64)?;
            }
            for line in String::from_utf8_lossy(&data[..good]).lines() {
                let mut it = line.split_whitespace();
                let (Some(h), Some(v)) = (it.next(), it.next()) else { continue };
                if let Ok(h) = h.parse::<Height>() {
                    entries.insert(h, v.to_string());
                    last = last.max(h);
                }
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(HeightLog { file, last, entries })
    }

    /// Append `line` (without newline) for height `h` if `h > last`.
    /// Returns false if the height was already logged.
    pub fn append(&mut self, h: Height, line: &str) -> io::Result<bool> {
        if h <= self.last {
            return Ok(false);
        }
        let mut s = String::with_capacity(line.len() + 1);
        s.push_str(line);
        s.push('\n');
        self.file.write_all(s.as_bytes())?;
        self.last = h;
        // keep memory bounded: only recent entries are needed for verification
        if self.entries.len() > 20_000 {
            let cut = h.saturating_sub(10_000);
            self.entries = self.entries.split_off(&cut);
        }
        Ok(true)
    }

    pub fn get(&self, h: Height) -> Option<&str> {
        self.entries.get(&h).map(|s| s.as_str())
    }
}

pub fn hex32(h: &Hash32) -> String {
    hex::encode(h.0)
}

pub fn commit_line(h: Height, id: &BlockId, ts: TimestampMs) -> String {
    format!("{h} {} {ts}", hex32(id))
}

pub fn root_line(h: Height, root: &Hash32, order_actions: u64) -> String {
    format!("{h} {} {order_actions}", hex32(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn torn_tail_and_reopen() {
        let d = std::env::temp_dir().join(format!("hlnode-logs-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("roots.log");
        let _ = std::fs::remove_file(&p);
        {
            let mut l = HeightLog::open(&p).unwrap();
            assert!(l.append(1, &root_line(1, &Hash32([1; 32]), 3)).unwrap());
            assert!(l.append(2, &root_line(2, &Hash32([2; 32]), 0)).unwrap());
            assert!(!l.append(2, "2 x 0").unwrap());
        }
        // torn write
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"3 abc").unwrap();
        drop(f);
        let mut l = HeightLog::open(&p).unwrap();
        assert_eq!(l.last, 2);
        assert_eq!(l.get(1), Some(hex32(&Hash32([1; 32])).as_str()));
        assert!(l.append(3, "3 ff 0").unwrap());
        drop(l);
        let s = std::fs::read_to_string(&p).unwrap();
        assert_eq!(s.lines().count(), 3);
        assert!(s.ends_with("3 ff 0\n"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
