# book

Per-market limit order book and trigger-order store for hlbft. Pure,
single-threaded, deterministic, integer-only.

## Public API

* `Book` — implements `interfaces::OrderBook`.
  * `Book::new(asset)`, `Book::with_capacity(asset, n)`
  * trait: `place`, `cancel`, `get`, `get_by_cloid`, `best_bid/ask`,
    `levels`, `user_orders`, `len`, `encode`
  * extras: `Book::decode(bytes)`, `would_cross(side, px)`,
    `reduce_size(oid, new_sz)` (size decrease keeping queue position),
    `orders_in_priority(side)`, `check_invariants()` (tests/debug)
* `NotionalAcc` — exact Σ px·sz accumulator; `avg_px()` rounds down.
* `TriggerBook` — implements `interfaces::TriggerStore`.
  extras: `TriggerBook::decode`, `children_of(parent)`, `update_sz`, `iter`,
  `check_invariants`; helpers `fires_on_rise`, `condition_holds`.
* `FxMap` — fixed-hasher map used for point lookups only.

## Matching semantics (`Book::place`)

* Strict price-time priority; fill price = maker price; candidate size =
  min(taker remaining, maker remaining). Every candidate goes through
  `MatchHook::on_match`:
  * `Fill`, accepted `a` (must be in `(0, sz]`, else panic): both sides reduced
    by `a`; matching continues (a partially accepted maker is re-offered).
  * `CancelMaker(reason)`: maker removed, listed in `canceled_makers`;
    continue.
  * `StopTaker`: stop; taker remainder **canceled whatever the TIF** (it would
    cross the book; see `INTERFACE_REQUESTS.md`).
* After matching: GTC rests the remainder (keeps `orig_sz`), IOC cancels it
  (`taker_canceled`). A fully filled order is never `taker_canceled`.
* ALO that would cross: `alo_rejected = taker_canceled = true`, the hook is
  never called; otherwise it rests.
* `avg_px` = floor(Σ px·sz / Σ sz), computed from the exact u128 sum.
* Duplicate oid / wrong asset / non-positive size panic (caller bugs).
* cloid index is per user; if a user rests two orders with the same cloid the
  newest wins the lookup (the state crate is expected to reject duplicates).

## Data structure

Slab (`Vec<Slot>` + free list) of orders with intrusive prev/next links per
price level; `BTreeMap<Px, Level>` per side (head/tail/total/count);
`FxMap<Oid, slot>`, `FxMap<(Address, Cloid), Oid>`,
`FxMap<Address, BTreeSet<Oid>>`. Hash maps are only used for point lookups;
anything ordered (levels, user listings, encoding) comes from BTreeMaps or the
FIFO links. Cancel is O(1) unlink + O(log L).

## Encoding

`encode()` = borsh `{version: u8 = 1, asset, bids, asks}` where `bids`/`asks`
are the resting orders in priority order (best price first, FIFO within a
level). Slab layout, free list and hash maps are not encoded, so the same
logical book always yields the same bytes. `decode` rejects non-canonical
input (unsorted, crossed, wrong side/asset, duplicate oid, zero size, IOC,
trailing bytes), so `decode(b).encode() == b`.

`TriggerBook::encode()` = borsh `{version: u8 = 1, asset, orders by oid}`.

## Trigger semantics

TP sell / SL buy fire when `mark >= trigger_px`; TP buy / SL sell when
`mark <= trigger_px`. Orders with `parent_oid` set are parked but ineligible
until `release_children(parent)`; `remove_children(parent)` drops them.
`take_triggered(mark)` returns triggered orders by oid, using per-direction
`BTreeSet<(trigger_px, oid)>` indexes (O(k log n)).

## Tests

```
cargo test -p book
# throughput benchmark (10k-order book, 50/50 place/cancel, 10% aggressive IOC)
cargo test --release -p book --test bench -- --ignored --nocapture
```

* `tests/book_unit.rs` — TIFs, partials, multi-level sweeps, priority,
  cancel/replace, reduce_size, hook decisions, levels, cloid, encoding.
* `tests/book_prop.rs` — proptest vs a naive Vec-based reference book with a
  scripted random hook (fills, partial accepts, maker cancels, stops); checks
  outcomes, candidate sequences, levels, user orders, lookups and encoding
  after every op; decoded book behaves identically.
* `tests/trigger.rs` — trigger directions, parent/child, encoding, proptest
  vs brute-force scan.
* `tests/bench.rs` — ~4.3M ops/s measured (release, single thread).
