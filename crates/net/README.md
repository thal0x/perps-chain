# net — validator p2p transport

`net::P2p` implements `interfaces::Network` over TCP: an authenticated full
mesh between the validators of a `ValidatorSet`, borsh framing, per-peer
bounded outbound queues and a runtime-configurable chaos shim.

## API

```rust
let (p2p, inbound) = net::P2p::start(NetConfig {
    chain_id, secret_key /* [u8;32] validator key */, validators, ..Default::default()
})?;                                   // or start_with_listener(cfg, std_listener)
p2p.broadcast(NetMsg::Consensus(..)); // to every other validator, encoded once
p2p.send(peer, msg);                   // send(local, ..) loops back to our own inbound
p2p.peers();                           // currently connected (authenticated) peers
inbound.consensus.recv();              // crossbeam Receiver<(Address, NetMsg)>, unbounded
inbound.txs.recv();                    // crossbeam Receiver<(Address, NetMsg)>, bounded
p2p.set_chaos(ChaosConfig { .. })?;    // or set_chaos_json(r#"{"partitions":[..]}"#)
p2p.metrics();                         // BTreeMap<Address, PeerMetrics>
p2p.shutdown();                        // also on Drop
```

* **Runtime**: `P2p` owns a private multi-thread tokio runtime
  (`worker_threads`, default 2) on its own thread, so it works from sync code
  and from inside other runtimes. `shutdown()` blocks until it exits.
* **Inbound channels are crossbeam**. `NetMsg::Consensus` → `consensus`
  (unbounded, never dropped). `NetMsg::Txs` → `txs` (bounded by
  `inbound_tx_capacity`, default 4096 batches; overflow is dropped and counted
  as `inbound_tx_dropped`), so a tx flood cannot starve consensus. Feed
  `txs` into `mempool::Pool::handle_peer_txs` from one or more threads.
* `from` is always the authenticated validator address.

## Design

* **Mesh**: the lower validator *address* dials the higher one; the higher
  side only accepts. Dialers reconnect with exponential backoff
  (`backoff_min_ms` 50 → `backoff_max_ms` 2000, plus jitter, reset on
  success). A new authenticated connection from a peer replaces the old one
  (peer restarted).
* **Handshake** (`handshake.rs`): both sides send
  `MAGIC || version || chain_id || claimed address || 32-byte nonce`, check the
  chain id and that the claimed address is an expected validator, then each
  signs `keccak256("hlbft-handshake" || chain_id u64 BE || peer_nonce)` with
  secp256k1 (65-byte r‖s‖v); the receiver recovers the signer and requires it
  to equal the claimed address. Non-validators, wrong keys and wrong chain ids
  are dropped before any message is accepted. No transport encryption.
* **Framing**: `u32 BE length || borsh(NetMsg)`, max 32 MiB
  (`max_frame_bytes`). Oversize outbound messages are dropped locally
  (`oversize_dropped()`); oversize/undecodable inbound frames close the link.
* **Outbound queue** (`queue.rs`), one per connection, two FIFOs: consensus
  frames are written first. Tx frames are bounded by `tx_queue_max_bytes`
  (64 MiB) with drop-oldest. Consensus frames are never dropped; if they
  exceed `consensus_queue_max_bytes` (1 GiB) the peer is not reading and the
  connection is reset. Messages to a disconnected peer are dropped
  (`dropped_disconnected`) rather than buffered — consensus recovers via
  sync/timeouts.
* **Chaos shim** (`chaos.rs`, JSON via serde, all fields optional):
  `partitions` (groups; traffic only inside a group; a node listed in no
  group is isolated), `drop_prob`, `delay_ms_min/max` (uniform),
  `reorder_prob` + `reorder_delay_ms_max` (extra uniform delay),
  `duplicate_prob`, `links: [{to, blocked, drop_prob?, delay_ms_min?, ...}]`
  per-destination overrides, `seed` (ChaCha8, mixed with the local address).
  Drop/delay/duplicate apply on the outbound path per destination.
  Partitions and `blocked` links are enforced **both outbound and inbound**,
  and re-checked just before each socket write, so one node's config alone
  fully isolates it, and a heal (`{}`) takes effect instantly.
  `ChaosConfig::isolate(node, &all)` is a helper.
* **Metrics** per peer: connected, connects, bytes/msgs in/out, tx frames
  dropped (backpressure), inbound tx drops, chaos drops in/out, queued bytes.

## Tests

```
cargo test -p net                    # unit + localhost integration tests
cargo test -p net --release --test mesh -- --ignored throughput --nocapture
```

Integration tests (`tests/mesh.rs`): 4-node mesh broadcast/send/loopback/FIFO,
reconnect after a dialer and an acceptor restart, handshake rejection
(impostor key, non-validator, wrong chain id), chaos partition isolate/heal,
2|2 split, per-link drop/block/delay, drop/duplicate statistics, 20 MiB
consensus and tx messages and >32 MiB refusal, tx flood vs consensus,
outbound backpressure. Throughput (ignored): ~1.2 GB/s on one localhost link
(target ≥ 50 MB/s).
