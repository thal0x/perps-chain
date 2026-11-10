//! Small deterministic (unkeyed) hasher for point-lookup indexes.
//!
//! The hash maps built on it are **only** used for point lookups
//! (`get`/`insert`/`remove`); nothing that feeds state results ever iterates
//! them, so their internal order is irrelevant to determinism. A fixed hasher
//! (instead of std's randomly keyed SipHash) additionally makes performance
//! reproducible and is noticeably faster for small integer keys.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

const K: u64 = 0x517c_c1b7_2722_0a95;

/// FxHash (as used by rustc): rotate, xor, multiply.
#[derive(Default, Clone, Copy)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, w: u64) {
        self.hash = (self.hash.rotate_left(5) ^ w).wrapping_mul(K);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().expect("8 bytes")));
        }
        let rem = chunks.remainder();
        if !rem.is_empty() {
            let mut buf = [0u8; 8];
            buf[..rem.len()].copy_from_slice(rem);
            self.add(u64::from_le_bytes(buf));
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

/// Point-lookup-only hash map with a fixed hasher. Never iterate it in a way
/// that affects state results.
pub type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;
