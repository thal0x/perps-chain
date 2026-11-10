# api — HL-compatible HTTP + WebSocket server

Serves the Hyperliquid API shape so the official Python SDK and the TS SDK
`@nktkas/hyperliquid` run unmodified against hlbft (only the base URL changes).

## Public API

```rust
api::serve(backend: Arc<dyn ApiBackend>, addr: SocketAddr) -> impl Future<Output = io::Result<()>>
api::serve_with(backend, addr, ApiConfig { exchange_timeout, ws_buffer, ws_max_subs, limits, receipt_cache, max_body })
api::Shared::start(backend, cfg) -> Arc<Shared>      // indexer thread + on_block registration
api::serve_listener(shared, tokio::net::TcpListener)  // e.g. port 0 in tests
api::router(shared) -> axum::Router
api::Indexer / IndexerLimits                          // history store (usable standalone)
api::testkit::{FakeBackend, FakeState, Script, test_genesis, simple_responder}
```

Routes: `POST /info`, `POST /exchange`, `GET /ws` (also `GET /` upgrade),
`GET /admin/status` (NodeStatus JSON), `POST /admin/chaos` (body ->
`ApiBackend::set_chaos`). CORS is permissive. Supported request types and
channels: see [COMPAT.md](COMPAT.md).

## Design

* **Indexer thread.** `ApiBackend::on_block` only pushes the
  `Arc<BlockOutcome>` into a channel. One `api-indexer` thread then does three
  things per block, in order. First it indexes the block (fills, order
  lifecycle, funding, ledger, trades, candles for all 14 intervals). Then it
  resolves `/exchange` waiters, so a client's follow-up `/info` already sees
  the block. Last it publishes the WebSocket pushes. Memory is bounded:
  * per user: 2000 fills, 2000 orders, 1000 funding, 1000 ledger entries;
  * a global record cap and a user cap, enforced by LRU eviction of whole
    users;
  * per coin: trades and funding points;
  * 5000 candles per series.
* **Exchange.** The body bytes go through `wire::parse_exchange_bytes`, a
  zero-copy JSON→msgpack fast path. It falls back to
  `hlwire::parse_exchange_request` for anything unusual, and is
  differential-tested against hlwire (SDK vectors + proptest). The flow:
  1. compute `Tx::hash()`;
  2. register a oneshot waiter in a 64-shard map *before* `submit`, so there
     is no race with fast blocks;
  3. submit and wait up to 10 s for the receipt.

  Responses are rendered straight into a string, with no `serde_json::Value`
  on the hot path. Duplicates (`AdmissionError::Duplicate`) are answered from
  a bounded recent-receipt cache, or by waiting for the original. Admission
  errors map to HL texts (`User or API Wallet 0x.. does not exist.`, ...).
* **WebSocket.** Each connection has a bounded outbound queue
  (`ws_buffer`, default 4096 messages); a full queue kills the connection
  (slow consumer). Subscriptions live in a hub keyed by canonical `SubKey`.
  Each push is serialized once per key and fanned out. Subscribing holds the
  hub's publish lock while it sends the snapshot and registers the
  subscription. Each block is therefore delivered exactly once, after the
  snapshot (`userFills` and others start with `isSnapshot: true`).
  `post` requests of type `info` are answered inline. `action` requests run in
  a task, so other traffic is not blocked. The greeting is
  `Websocket connection established.`, as on HL.
* **Formatting.** Decimals follow HL style ("1100.0", "0.0", "1891.4"):
  trimmed, but whole numbers keep one fractional digit. Addresses and hashes
  are lowercase hex.
* **Robustness.** Handlers never panic on input: bad `/info` requests get 422
  with text, as on HL, and bad `/exchange` requests get
  `{"status":"err",...}`. A panic on one block is caught, and the indexer
  keeps going.

## Tests

```
cargo test -p api
```
* unit tests: formatting, candle buckets (incl. 1w Monday and 1M calendar), dir strings, fast render == Value render;
* `tests/http.rs`: every /info type, exchange flows (resting, filled, errors, cancel, default, err, admission, duplicate, timeout, 400 concurrent 50-order batches), admin, CORS, pendingWithdrawals round-trip through `relayer::api::parse_pending_withdrawals`, and real hlwire-signed requests;
* `tests/ws.rs`: all channels, snapshots, ping, errors, unsubscribe, `post` (info and action), `/` upgrade, cleanup;
* `tests/wire.rs`: fast parser == hlwire on SDK vectors, edge cases and proptest;
* `tests/schema.rs`: every JSON shape emitted (126 samples, 67 SDK types: /info, /exchange, WS events) is validated against the TS SDK's TypeScript response types, converted to JSON Schema with `additionalProperties: false` the way the SDK's own tests do (node + ajv; `tests/schema/`);
* `tests/python_sdk.rs`: the real Python SDK (`HL_PYTHON` = a python with hyperliquid-python-sdk installed) runs `Info`, `Exchange`, `market_open`/`close`, tpsl, transfers, agent and the `basic_ws` subscriptions. Every signature is recovered with hlwire;
* `tests/ts_sdk.rs`: `@nktkas/hyperliquid` 0.33.3 from npm runs the README examples over HTTP and WebSocket (`tests/ts/`).

The node/python tests skip with a message if their runtime is missing.

Benchmarks: `cargo run --release -p api --example bench_exchange -- 64 5`
measures `/exchange` req/s with 50-order batches, with the client in the same
process. It measured ≈17k req/s on the shared 4-core box. `bench_parts`
reports the cost of each pipeline stage. `examples/fake_server.rs` serves the
fake backend for manual SDK poking.
