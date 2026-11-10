# mempool — transaction pool

`mempool::Pool` implements `interfaces::Mempool`.

## API

```rust
let pool: Arc<Pool> = Pool::new(PoolConfig::default(),
    PoolDeps::new(Arc::new(move |tx| app.check_tx(tx)))   // AdmissionChecker
        .with_network(p2p.clone())                         // optional: gossip
        .with_signer_of(Arc::new(|tx| cache.signer(tx)))   // optional: per-signer limit
        .with_clock(clock));                               // optional: tests
pool.insert(tx, TxSource::Api { ip })      // Mempool trait: full pipeline
pool.insert_batch(txs, source)             // many txs, callable from many threads
pool.insert_prechecked(tx, source, signer) // caller already ran check_tx (+ recovery)
pool.handle_peer_txs(from, txs)            // gossip from peers (never re-gossiped)
pool.reap(max_txs, max_bytes)              // validator txs first, then FIFO; no removal
pool.remove_committed(&hashes)             // remove + remember hashes, expire stale txs
pool.recheck() / pool.retain(|tx| ..)      // evict txs invalidated by new state
pool.flush_gossip(); pool.stats(); pool.prune();
```

## Admission pipeline (per tx)

1. borsh-encode once → blake3 hash (= `Tx::hash()`) and size.
2. Dedup against the pool **and** recently committed hashes
   (`recent_committed_ttl_ms` 120 s, at most `recent_committed_max`), so
   re-gossip of committed txs is `Duplicate`.
3. Per-IP token bucket for `TxSource::Api` (2000/s, burst 4000). Taken
   before the check, so invalid floods also cost tokens.
4. Capacity (200k txs / 512 MiB): user txs get `MempoolFull`; validator txs
   always enter, evicting the oldest user txs.
5. Admission checker (outside all locks).
6. Per-signer token bucket (1000/s, burst 2000) for API user txs (and peer
   txs if `signer_limit_peer_txs`). **Signer source**: the `signer` argument
   of `insert_prechecked`, else the optional `SignerOf` callback (run after a
   successful check — e.g. a recovery cache the node's `check_tx` fills keyed
   by tx hash), else no signer limit. Validator txs are exempt.
7. Insert into the shard; API/Local txs are buffered for gossip.

Gossip: if a network is given and `gossip_interval_ms > 0` (10 ms), a
background thread broadcasts buffered txs as `NetMsg::Txs` batches
(≤ 10k txs / 4 MiB each); it exits when the pool is dropped. With interval 0
call `flush_gossip()` yourself.

## Design

Shards (32, by tx hash) each behind a `parking_lot::RwLock`: a hash → seq
map, and two `BTreeMap<seq, slot>` FIFOs (validator / user). A global atomic
seq gives the FIFO order; `reap` holds read locks on all shards and k-way
merges the FIFOs (validators first). A tx that does not fit the remaining
byte budget is skipped. Txs older than `tx_ttl_ms` (120 s) are expired on
`remove_committed`/`prune`. Rate limiters are sharded token buckets with
integer milli-token arithmetic and an injected `Clock`.

Perf (release, this box, `perf_insert_and_reap`): ~860k inserts/s on one
thread with a trivial checker (target 300k), reap 20k of 200k ≈ 3.8 ms
(target < 5 ms). Signature recovery (~25 µs) is the node's cost; run it in
parallel threads calling `insert_batch`, or verify in a rayon stage and call
`insert_prechecked`.

## Tests

```
cargo test -p mempool
cargo test -p mempool --release --test pool -- --ignored perf --nocapture
```

Dedup + committed memory, checker errors, per-IP and per-signer limits with
a manual clock, capacity (count and bytes) and validator eviction, reap
ordering/limits/skip, removal/TTL/retain/recheck, gossip batching (count and
bytes) with a fake `Network`, timer gossip, multi-thread stress.
