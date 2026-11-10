//! Throughput benchmark (ignored by default). Run with:
//! `cargo test --release -p book --test bench -- --ignored --nocapture`

mod common;
use book::*;
use common::*;
use interfaces::*;
use std::time::Instant;

struct Trivial;
impl MatchHook for Trivial {
    #[inline]
    fn on_match(&mut self, c: &CandidateFill<'_>) -> (MatchDecision, Sz) {
        (MatchDecision::Fill, c.sz)
    }
}

/// xorshift for workload generation (test-only).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn mk(oid: Oid, user: u8, side: Side, p: u64, s: i64, tif: Tif) -> RestingOrder {
    RestingOrder {
        oid,
        user: addr(user),
        asset: 0,
        side,
        limit_px: Px(p),
        sz: Sz(s),
        orig_sz: Sz(s),
        reduce_only: false,
        cloid: None,
        timestamp: 0,
        tif,
    }
}

#[test]
#[ignore]
fn bench_place_cancel_10k_book() {
    const BOOK: usize = 10_000;
    const OPS: usize = 4_000_000;
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut b = Book::with_capacity(0, BOOK * 2);
    let mid: u64 = 1_000_000;
    let mut live: Vec<Oid> = Vec::with_capacity(BOOK * 2);
    let mut oid: Oid = 1;
    // Pre-fill: bids below mid, asks above, 500 levels each side.
    while live.len() < BOOK {
        let side = if oid % 2 == 0 { Side::Bid } else { Side::Ask };
        let off = 1 + rng.next() % 500;
        let p = if side == Side::Bid { mid - off } else { mid + off };
        b.place(mk(oid, (oid % 200) as u8, side, p, 1 + (rng.next() % 100) as i64, Tif::Gtc), &mut Trivial);
        live.push(oid);
        oid += 1;
    }
    let mut hook = Trivial;
    let mut places = 0u64;
    let mut cancels = 0u64;
    let mut fills = 0u64;
    let t0 = Instant::now();
    for i in 0..OPS {
        let r = rng.next();
        // Alternate place/cancel, but keep the book at >= BOOK orders.
        if i % 2 == 0 || live.len() < BOOK {
            // place: 90% passive GTC, 10% small aggressive IOC
            let side = if r & 1 == 0 { Side::Bid } else { Side::Ask };
            let aggressive = (r >> 1) % 10 == 0;
            let off = 1 + (r >> 8) % 500;
            let p = match (side, aggressive) {
                (Side::Bid, false) => mid - off,
                (Side::Ask, false) => mid + off,
                (Side::Bid, true) => mid + 1 + (r >> 30) % 3,
                (Side::Ask, true) => mid - 1 - (r >> 30) % 3,
            };
            let tif = if aggressive { Tif::Ioc } else { Tif::Gtc };
            let o = b.place(mk(oid, (r % 200) as u8, side, p, 1 + ((r >> 20) % 100) as i64, tif), &mut hook);
            fills += o.filled_makers.len() as u64;
            if o.resting_sz.0 > 0 {
                live.push(oid);
            }
            oid += 1;
            places += 1;
        } else {
            // cancel a random live order (swap-remove)
            if live.is_empty() {
                continue;
            }
            let k = (r as usize) % live.len();
            let victim = live.swap_remove(k);
            if b.cancel(victim).is_some() {
                cancels += 1;
            }
        }
    }
    let ns = t0.elapsed().as_nanos().max(1);
    let ops = places + cancels;
    let per_sec = ops as u128 * 1_000_000_000 / ns;
    println!(
        "book bench: {ops} ops ({places} places, {cancels} cancels, {fills} maker fills) in {} ms => {per_sec} ops/s; final book {} orders",
        ns / 1_000_000,
        b.len()
    );
    b.check_invariants();
}
