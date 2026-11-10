//! Incremental line tail of an append-only file.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Reads complete lines appended since the last poll. A trailing partial
/// line is buffered until its newline arrives (or discarded with
/// [`LogTail::discard_partial`] after the writer was killed mid-write).
#[derive(Debug)]
pub struct LogTail {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
    /// Number of times the file shrank (truncated / replaced) under us.
    pub truncations: u64,
    /// Bytes of torn (never completed) lines discarded.
    pub torn_discarded: u64,
}

impl LogTail {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self { path: path.as_ref().to_path_buf(), offset: 0, partial: vec![], truncations: 0, torn_discarded: 0 }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// New complete lines (without the trailing newline). A missing file
    /// yields nothing. Reads at most `max_bytes` per call.
    pub fn poll_max(&mut self, max_bytes: u64) -> Vec<String> {
        let mut f = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return vec![],
        };
        let len = match f.metadata() {
            Ok(m) => m.len(),
            Err(_) => return vec![],
        };
        if len < self.offset {
            self.truncations += 1;
            self.offset = 0;
            self.partial.clear();
        }
        if len == self.offset {
            return vec![];
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return vec![];
        }
        let want = (len - self.offset).min(max_bytes);
        let mut buf = Vec::with_capacity(want as usize);
        if f.take(want).read_to_end(&mut buf).is_err() {
            return vec![];
        }
        self.offset += buf.len() as u64;
        let mut out = vec![];
        let mut start = 0;
        for (i, b) in buf.iter().enumerate() {
            if *b == b'\n' {
                let mut line = std::mem::take(&mut self.partial);
                line.extend_from_slice(&buf[start..i]);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                out.push(String::from_utf8_lossy(&line).into_owned());
                start = i + 1;
            }
        }
        self.partial.extend_from_slice(&buf[start..]);
        out
    }

    pub fn poll(&mut self) -> Vec<String> {
        let mut all = vec![];
        loop {
            let v = self.poll_max(8 << 20);
            if v.is_empty() {
                // Either nothing new, or a chunk without any newline: check
                // whether we are at EOF before giving up.
                let at_eof = std::fs::metadata(&self.path).map(|m| m.len() <= self.offset).unwrap_or(true);
                if at_eof {
                    return all;
                }
                continue;
            }
            all.extend(v);
        }
    }

    /// Drop a buffered partial line (the writer died before finishing it)
    /// and rewind to its start: hlnode truncates a torn last line on
    /// restart, so the next complete line is appended at that offset.
    pub fn discard_partial(&mut self) -> Option<String> {
        if self.partial.is_empty() {
            return None;
        }
        self.torn_discarded += self.partial.len() as u64;
        self.offset -= self.partial.len() as u64;
        let s = String::from_utf8_lossy(&self.partial).into_owned();
        self.partial.clear();
        Some(s)
    }

    pub fn has_partial(&self) -> bool {
        !self.partial.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn tails_lines_and_partials() {
        let dir = crate::testutil::tempdir("tail");
        let p = dir.join("f.log");
        let mut t = LogTail::new(&p);
        assert!(t.poll().is_empty());
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p).unwrap();
        f.write_all(b"1 a\n2 b\n3 ").unwrap();
        assert_eq!(t.poll(), vec!["1 a", "2 b"]);
        assert!(t.has_partial());
        f.write_all(b"c\n").unwrap();
        assert_eq!(t.poll(), vec!["3 c"]);
        f.write_all(b"4 torn").unwrap();
        assert!(t.poll().is_empty());
        assert_eq!(t.discard_partial().as_deref(), Some("4 torn"));
        // the restarted writer truncates the torn tail, then appends
        drop(f);
        let len = std::fs::metadata(&p).unwrap().len();
        std::fs::OpenOptions::new().write(true).open(&p).unwrap().set_len(len - 6).unwrap();
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"4 d\n5 e\n").unwrap();
        assert_eq!(t.poll(), vec!["4 d", "5 e"]);
        // truncation resets
        drop(f);
        std::fs::write(&p, b"9 z\n").unwrap();
        assert_eq!(t.poll(), vec!["9 z"]);
        assert_eq!(t.truncations, 1);
    }
}
