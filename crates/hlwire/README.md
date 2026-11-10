# hlwire

Hyperliquid wire compatibility for hlbft: msgpack, action decoding, L1 and
user-signed (EIP-712) signatures, `/exchange` request parsing/building,
wallets, validator/consensus digests. Exact JSON shapes: [WIRE.md](WIRE.md).

## Public API

| area | items |
|---|---|
| msgpack | `json_to_msgpack` (= `action_to_msgpack`), `msgpack_to_json` |
| decode | `decode_action` (fast path), `decode_action_generic` (reference), `action_type`, `is_user_signed_type`, `is_user_signed(&Action)` |
| wire helpers | `parse_cloid`, `cloid_to_hex`, `cloid_from_u128`, `parse_address`, `parse_wire_decimal`, `parse_px`, `parse_sz`, `parse_usd`, `parse_usd_amount` |
| L1 signing | `l1_action_hash`, `l1_signing_digest`, `sign_l1_action` |
| user-signed | `user_signed_digest`, `sign_user_signed_action`, `user_signed_types`, `user_signed_nonce_field`, `hyperliquid_chain`, `DEFAULT_SIGNATURE_CHAIN_ID` |
| requests | `parse_exchange_request_bytes` (raw body text; fast path, use this in the API), `parse_exchange_request_fast`, `parse_exchange_request`, `build_exchange_request`, `parse_signature`, `sig_to_json`, `signed_l1_request`, `signed_user_request`, `user_tx_from_parts` |
| verify | `recover`, `verify`, `recover_signer(&UserTx, is_mainnet)`, `HlVerifier { is_mainnet }` (`impl interfaces::TxVerifier`) |
| keys | `Wallet::{from_secret_hex, from_bytes, random(rng), address, sign_digest, secret_bytes, secret_hex}` |
| validators | `validator_tx_digest`, `sign_validator_tx`, `recover_validator_tx`, `consensus_digest`, `keccak256` |
| builders | `order_to_wire_json`, `order_to_wire_json_checked(o, sz_decimals)`, `order_action`, `cancel_action`, `cancel_by_cloid_action`, `modify_action`, `batch_modify_action`, `schedule_cancel_action`, `update_leverage_action`, `update_isolated_margin_action`, `set_referrer_action`, `noop_action`, `usd_send_action`, `spot_send_action`, `withdraw3_action`, `usd_class_transfer_action`, `approve_agent_action`, `action_to_json` |

Typical API flow:

```rust
let body: serde_json::Value = serde_json::from_slice(&bytes)?;      // preserve_order
let tx = hlwire::parse_exchange_request(&body)?;                    // UserTx (msgpack bytes)
let v = hlwire::HlVerifier { is_mainnet: false };
let action = v.decode(&tx)?;                                        // interfaces::Action
let signer = v.recover_signer(&tx, &action)?;                       // user or agent address
```

Simulator flow:

```rust
let w = hlwire::Wallet::random(&mut chacha_rng);
let body = hlwire::signed_l1_request(&w, hlwire::order_action(&orders, Grouping::Na), nonce, None, None, false);
let tx = hlwire::parse_exchange_request(&body)?;
```

## Design notes

* `serde_json` has `preserve_order` + `arbitrary_precision`; a number is a
  float iff its token has `.`/`e`/`E` (encoded float64), otherwise the
  smallest msgpack int. Ints beyond 64 bits are an error.
* `decode_action`: for order/cancel/cancelByCloid/modify/batchModify an
  allocation-free single pass (`src/fast.rs`) validates and decodes at once,
  parsing px/sz/cloids straight from the msgpack string bytes (the only
  allocation is the output `Vec`). Other types use the reference decoder
  `decode_action_generic` (validate once, then lazy views). Proptests check
  fast == reference on random, mutated and truncated input.
* `parse_exchange_request_bytes(body)` transcodes the JSON text straight to
  msgpack (`src/json.rs`), no `serde_json::Value`. Anything unusual
  (escaped/duplicate keys, floats, deep nesting, any error) falls back to
  `serde_json::from_slice` + `parse_exchange_request`, so results and errors
  are identical by construction (also proptested). Decimal strings are parsed exactly (no floats) with
  scientific notation accepted (Python `str(float)` amounts like `"1e-05"`).
* Recovery uses `secp256k1` (global context); signing is RFC 6979 so our
  signatures are byte-identical to eth_account / viem (asserted by tests).
* User-signed verification enforces: `hyperliquidChain` matches the chain,
  `expiresAfter` absent, tx nonce == action `time`/`nonce`.
* No floating point is used on any decode/verify path (float msgpack values
  are carried as raw bits and rejected where a number is expected).

## Tests

```
cargo test -p hlwire                       # unit + golden + proptests
cargo test -p hlwire --release --test bench -- --ignored --nocapture   # timings
HLWIRE_BENCH_STRICT=1 ...                  # make timing targets fail the bench
```

Golden vectors (`testdata/python_vectors.json`, `testdata/ts_vectors.json`)
are regenerated with:

```
python3 -m venv /home/user/.venvs/hl && /home/user/.venvs/hl/bin/pip install -e /home/user/ext/hyperliquid-python-sdk
/home/user/.venvs/hl/bin/python -I testdata/gen_python_vectors.py > testdata/python_vectors.json
mkdir /home/user/.hlts && cd /home/user/.hlts && npm install @nktkas/hyperliquid viem
HLTS_DIR=/home/user/.hlts node testdata/gen_ts_vectors.mjs > testdata/ts_vectors.json
```

For every vector the tests assert: identical msgpack bytes, msgpack->JSON
roundtrip, identical action hash / EIP-712 digest, `recover` == signer,
`HlVerifier` == signer, our wallet re-creates the identical signature, typed
decode (+ spot checks), builder roundtrip, and that tampering / the wrong
chain breaks recovery.

Bench (shared 4-core box under load ~10, CPU time, release):
`decode_action` 50 orders ~7-10 us (was ~28-42 us; 307K -> 107K
instructions under callgrind), `parse_exchange_request_bytes` for the
50-order body ~20 us (serde_json Value path ~100 us; 980K -> 265K
instructions), `recover` ~39-55 us depending on load (libsecp256k1, ~1.0 M
instructions/op), `sign_digest` ~25-40 us.
