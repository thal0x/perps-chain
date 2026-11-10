//! Per-connection bounded outbound queue.
//!
//! Two FIFOs: consensus frames and tx-gossip frames. The writer always drains
//! consensus frames first (priority), so a tx flood cannot delay votes.
//! Bounds:
//! * tx frames: `tx_max_bytes`; on overflow the *oldest* tx frames are
//!   dropped (the newest one is always kept);
//! * consensus frames are never dropped; if they exceed `consensus_max_bytes`
//!   the connection is considered dead (peer not reading) and `push` returns
//!   `Overflow` so the caller closes it (the peer then catches up via sync
//!   after reconnecting).

use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::VecDeque;
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushResult {
    Ok { dropped_tx_frames: usize },
    Overflow,
    Closed,
}

#[derive(Default)]
struct Inner {
    cons: VecDeque<Bytes>,
    txs: VecDeque<Bytes>,
    cons_bytes: usize,
    tx_bytes: usize,
    closed: bool,
}

pub struct OutQueue {
    inner: Mutex<Inner>,
    notify: Notify,
    tx_max_bytes: usize,
    consensus_max_bytes: usize,
}

impl OutQueue {
    pub fn new(tx_max_bytes: usize, consensus_max_bytes: usize) -> Self {
        Self { inner: Mutex::new(Inner::default()), notify: Notify::new(), tx_max_bytes, consensus_max_bytes }
    }

    pub fn push(&self, frame: Bytes, is_tx: bool) -> PushResult {
        let mut q = self.inner.lock();
        if q.closed {
            return PushResult::Closed;
        }
        let mut dropped = 0;
        if is_tx {
            q.tx_bytes += frame.len();
            q.txs.push_back(frame);
            while q.tx_bytes > self.tx_max_bytes && q.txs.len() > 1 {
                let f = q.txs.pop_front().unwrap();
                q.tx_bytes -= f.len();
                dropped += 1;
            }
        } else {
            if q.cons_bytes + frame.len() > self.consensus_max_bytes && !q.cons.is_empty() {
                return PushResult::Overflow;
            }
            q.cons_bytes += frame.len();
            q.cons.push_back(frame);
        }
        drop(q);
        self.notify.notify_one();
        PushResult::Ok { dropped_tx_frames: dropped }
    }

    /// Wait for and take a batch of frames (consensus first) totalling at
    /// most ~`max_bytes` (at least one frame). Returns None once closed.
    pub async fn pop_batch(&self, max_bytes: usize, out: &mut Vec<Bytes>) -> bool {
        loop {
            {
                let mut q = self.inner.lock();
                if q.closed {
                    return false;
                }
                let mut total = 0;
                while total < max_bytes {
                    if let Some(f) = q.cons.pop_front() {
                        q.cons_bytes -= f.len();
                        total += f.len();
                        out.push(f);
                    } else if let Some(f) = q.txs.pop_front() {
                        q.tx_bytes -= f.len();
                        total += f.len();
                        out.push(f);
                    } else {
                        break;
                    }
                }
                if !out.is_empty() {
                    return true;
                }
            }
            self.notify.notified().await;
        }
    }

    pub fn close(&self) {
        let mut q = self.inner.lock();
        q.closed = true;
        q.cons.clear();
        q.txs.clear();
        q.cons_bytes = 0;
        q.tx_bytes = 0;
        drop(q);
        self.notify.notify_one();
    }

    pub fn queued_bytes(&self) -> usize {
        let q = self.inner.lock();
        q.cons_bytes + q.tx_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(n: usize, tag: u8) -> Bytes {
        Bytes::from(vec![tag; n])
    }

    #[tokio::test]
    async fn drops_oldest_txs_never_consensus_and_prioritises_consensus() {
        let q = OutQueue::new(100, 1000);
        assert_eq!(q.push(f(40, 1), true), PushResult::Ok { dropped_tx_frames: 0 });
        assert_eq!(q.push(f(40, 2), true), PushResult::Ok { dropped_tx_frames: 0 });
        assert_eq!(q.push(f(40, 3), true), PushResult::Ok { dropped_tx_frames: 1 });
        for i in 0..10 {
            assert_eq!(q.push(f(90, 100 + i), false), PushResult::Ok { dropped_tx_frames: 0 });
        }
        assert_eq!(q.push(f(200, 0), false), PushResult::Overflow);
        let mut out = vec![];
        assert!(q.pop_batch(usize::MAX, &mut out).await);
        let tags: Vec<u8> = out.iter().map(|b| b[0]).collect();
        assert_eq!(tags, vec![100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 2, 3]);
        // oversized single tx frame is kept
        assert_eq!(q.push(f(500, 9), true), PushResult::Ok { dropped_tx_frames: 0 });
        assert_eq!(q.queued_bytes(), 500);
        q.close();
        assert_eq!(q.push(f(1, 1), false), PushResult::Closed);
        out.clear();
        assert!(!q.pop_batch(10, &mut out).await);
    }

    #[tokio::test]
    async fn pop_waits_for_push() {
        let q = std::sync::Arc::new(OutQueue::new(100, 1000));
        let q2 = q.clone();
        let h = tokio::spawn(async move {
            let mut out = vec![];
            q2.pop_batch(10, &mut out).await;
            out.len()
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        q.push(f(5, 1), false);
        assert_eq!(h.await.unwrap(), 1);
    }
}
