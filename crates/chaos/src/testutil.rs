//! Test helpers (also used by integration tests).

use std::path::PathBuf;

/// A fresh, unique directory under the system temp dir.
pub fn tempdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let p = std::env::temp_dir().join(format!("hlchaos-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&p).expect("create tempdir");
    p
}
