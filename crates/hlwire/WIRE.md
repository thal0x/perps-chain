# HL `/exchange` wire format (as sent by the official SDKs)

Everything here was captured from the real SDKs (Python
`hyperliquid-python-sdk` @ 2fdb18f, TS `@nktkas/hyperliquid` 0.33.3 from npm)
by `testdata/gen_python_vectors.py` / `testdata/gen_ts_vectors.mjs`; the
captured bodies are in `testdata/*_vectors.json` and are asserted
byte-for-byte by `tests/golden.rs`. When in doubt, look at the vectors.

## 1. Request body

```jsonc
// Python (requests json=..., json.dumps defaults: ", " and ": " separators)
{"action": {...}, "nonce": 1700000000001,
 "signature": {"r": "0x..", "s": "0x..", "v": 27},
 "vaultAddress": null, "expiresAfter": null}
// TS (JSON.stringify): optional keys omitted when unset, different key order
{"action":{...},"signature":{"r":"0x..","s":"0x..","v":28},"nonce":1700000000044,
 "vaultAddress":"0x1719884eb866cb12b2287399b15f7db5e7d775ea","expiresAfter":1800000000001}
```

* `vaultAddress` / `expiresAfter`: absent, `null`, or a value. Python always
  sends both keys (`null` when unset); for `usdClassTransfer`/`sendAsset`
  Python sends `vaultAddress: null` even when a vault is configured.
* `signature.r` / `s`: `0x` hex. **Python may drop leading zeros** (it uses
  `to_hex(int)`, e.g. a 63-digit `s` in vector
  `usd_class_transfer_vault_suffix`). `v` is 27/28 from both SDKs; we also
  accept 0/1 (normalized to 27/28).
* `nonce`: integer ms. For user-signed actions it equals the action's
  `time`/`nonce` field (both SDKs); `HlVerifier` enforces this.
* API: parse the body with `serde_json` (`preserve_order` is on), then
  `hlwire::parse_exchange_request(&body) -> UserTx`. `UserTx.action_bytes` is
  `json_to_msgpack(body.action)`; that byte string is what goes on chain.

## 2. msgpack (L1 hash input)

`json_to_msgpack` == Python `msgpack.packb(action)` == TS
`encode(adjust(action))`:

* map order = JSON key order as posted (insertion order);
* ints: smallest encoding (pos fixint, u8 `cc`, u16 `cd`, u32 `ce`, u64 `cf`;
  neg fixint, i8 `d0`, i16 `d1`, i32 `d2`, i64 `d3`). TS widens numbers
  outside int32/uint32 to BigInt, which `@std/msgpack` writes as `cf`/`d3`, so
  both SDKs agree for every integer an action can hold
  (e.g. `ntli: -3000250000` -> `d3 ffffffff4d2bd170`, oid `2^40+7` -> `cf ...`);
* a JSON number with `.`/`e`/`E` is a float -> float64 `cb` (Python semantics;
  no SDK action contains floats);
* str: fixstr (<32 bytes), `d9`, `da`, `db`; arrays `9x`/`dc`/`dd`; maps
  `8x`/`de`/`df`; `null` -> `c0`; bools `c2`/`c3`.

## 3. L1 actions (phantom-agent signature)

```
connectionId = keccak(msgpack(action) || nonce_be8 || 0x00            (no vault)
                                                 || 0x01 || vault20   (vault)
                                                 [|| 0x00 || expiresAfter_be8])
digest = EIP-712  domain {name:"Exchange", version:"1", chainId:1337, verifyingContract:0x0}
                  Agent{string source, bytes32 connectionId}, source "a" mainnet / "b" testnet
```
`l1_action_hash`, `l1_signing_digest`, `sign_l1_action`. The signer may be the
user or an approved agent wallet (`approveAgent`): the recovered address is
the *signer*; principal resolution is the state machine's job. Localhost URLs
make both SDKs sign as testnet (`"b"`, `"Testnet"`).

Key order is identical in both SDKs for L1 actions (TS canonicalizes to its
schema order, which matches Python). Shapes (exact bytes in the vectors):

| type | action JSON | notes |
|---|---|---|
| order | `{"type":"order","orders":[OrderWire...],"grouping":"na"}` (+ `"builder":{"b":addr,"f":int}` after grouping) | grouping `"na"`/`"normalTpsl"`/`"positionTpsl"`; TS also `{"p":int}` (priority; decoded as `Na`) |
| OrderWire | `{"a":asset,"b":isBuy,"p":"px","s":"sz","r":reduceOnly,"t":T,"c":"0x<32 hex>"?}` | `c` omitted when no cloid. positionTpsl orders have `"s":"0"` |
| T (limit) | `{"limit":{"tif":"Gtc"\|"Ioc"\|"Alo"}}` | TS also `"FrontendMarket"` (decoded as Ioc) |
| T (trigger) | `{"trigger":{"isMarket":bool,"triggerPx":"px","tpsl":"tp"\|"sl"}}` | key order isMarket, triggerPx, tpsl |
| cancel | `{"type":"cancel","cancels":[{"a":asset,"o":oid}]}` | TS optional trailing `"f":true` (fast cancel; ignored) |
| cancelByCloid | `{"type":"cancelByCloid","cancels":[{"asset":asset,"cloid":"0x.."}]}` | TS optional `"f":true` |
| modify | `{"type":"modify","oid":oid\|"0x cloid","order":OrderWire}` | **TS only** (Python `modify_order` sends batchModify). TS optional trailing `"a":true` |
| batchModify | `{"type":"batchModify","modifies":[{"oid":oid\|"0x cloid","order":OrderWire}]}` | TS optional `"a":true` |
| scheduleCancel | `{"type":"scheduleCancel","time":ms}` or `{"type":"scheduleCancel"}` | no `time` = clear |
| updateLeverage | `{"type":"updateLeverage","asset":a,"isCross":bool,"leverage":int}` | |
| updateIsolatedMargin | `{"type":"updateIsolatedMargin","asset":a,"isBuy":bool,"ntli":int}` | `ntli` = signed USD * 1e6 (Python `float_to_usd_int`); Python always sends `isBuy: true` |
| setReferrer | `{"type":"setReferrer","code":"ASDF"}` | Python signs with vault=None |
| noop | `{"type":"noop"}` | nonce supplied by caller |

Decimal strings (`p`, `s`, `triggerPx`): Python `float_to_wire` (`"1100"`,
`"0.0001234"`, `"30000.5"`, max 8 decimals); TS normalizes any input
(`1100.0` -> `"1100"`, `"1.23450"` -> `"1.2345"`). Decoding is exact via
`interfaces::fixed` scales: more precision than Px/Sz (8) / Usd (6) is an
error, never rounded.

Cloids / addresses: Python sends cloids exactly as given (may be
**uppercase**, e.g. `0x...ABCDEF`) and addresses as given (often
checksummed); TS lowercases both. All hex is parsed case-insensitively.

**TS quirk (modify/batchModify)**: the TS schema `oid: union([UnsignedInteger,
Cloid])` runs `Number()` first, so a cloid string that is a safe integer
(e.g. `"0x...0005"`) is sent as the *number* `5`, i.e. a modify-by-oid.
Random 128-bit cloids stay strings. We decode exactly what is posted.

## 4. User-signed actions (EIP-712 `HyperliquidSignTransaction`)

```
domain  {name:"HyperliquidSignTransaction", version:"1",
         chainId: int(action.signatureChainId, 16), verifyingContract:0x0}
message HyperliquidTransaction:<Name>(fields...)   -- only these fields are signed
```

| type | primary type | signed fields (in order) |
|---|---|---|
| usdSend | `HyperliquidTransaction:UsdSend` | `string hyperliquidChain, string destination, string amount, uint64 time` |
| spotSend | `HyperliquidTransaction:SpotSend` | `string hyperliquidChain, string destination, string token, string amount, uint64 time` |
| withdraw3 | `HyperliquidTransaction:Withdraw` | `string hyperliquidChain, string destination, string amount, uint64 time` |
| usdClassTransfer | `HyperliquidTransaction:UsdClassTransfer` | `string hyperliquidChain, string amount, bool toPerp, uint64 nonce` |
| approveAgent | `HyperliquidTransaction:ApproveAgent` | `string hyperliquidChain, address agentAddress, string agentName, uint64 nonce` |

(`user_signed_types` also knows approveBuilderFee, tokenDelegate, sendAsset,
userDexAbstraction, convertToMultiSigUser for digest purposes; they do not
decode to an `Action`.)

`destination` is a **string** field: its exact case is signed (Python may
send it uppercase/checksummed). `agentAddress` is an `address` (case-free).

Posted shapes (key order differs between SDKs, irrelevant for the signature
but it is what the msgpack `action_bytes` contain):

```jsonc
// Python: user fields, then type, then signatureChainId/hyperliquidChain appended by the signer
{"destination":"0x5e9e..","amount":"12.5","time":1700000000002,"type":"usdSend","signatureChainId":"0x66eee","hyperliquidChain":"Testnet"}
{"destination":"0x5e9e..","amount":"1.25","token":"PURR:0xc4bf3f870c0e9465323c0b6ed28096c2","time":...,"type":"spotSend",...}
{"destination":"0x5e9e..","amount":"2.0","time":...,"type":"withdraw3",...}
{"type":"usdClassTransfer","amount":"10.0","toPerp":true,"nonce":...,"signatureChainId":"0x66eee","hyperliquidChain":"Testnet"}
{"type":"approveAgent","agentAddress":"0x1563915e194D8CfBA1943570603F7606A3115508","agentName":"bot1","nonce":...,...}
// TS: type, signatureChainId, hyperliquidChain, user fields, time/nonce
{"type":"usdSend","signatureChainId":"0x1","hyperliquidChain":"Testnet","destination":"0x5e9e..","amount":"12.5","time":1700000000002}
```

* `signatureChainId`: Python always `"0x66eee"`; TS defaults to the wallet's
  chain id, `"0x1"` for a viem local account (configurable). Any value is
  accepted; it only feeds the domain `chainId`.
* `hyperliquidChain`: `"Mainnet"`/`"Testnet"`; `HlVerifier` requires it to
  match the chain (`is_mainnet=false` locally).
* Amount strings: Python uses `str(float)` -> `"2.0"`, `"10.0"`, and for tiny
  values scientific notation (`"1e-05"`), which we accept exactly; TS sends
  normalized decimals (`"2"`).
* **approveAgent unnamed**: Python signs `agentName: ""` and then *deletes*
  the key from the posted action; TS sends `"agentName":""` (`null` is
  coerced to `""`). Our digest treats absent/null as `""`; decode maps
  absent/null/`""` to `agent_name: None`. Python's agent address is
  checksummed; TS lowercases.
* **usdClassTransfer with a vault**: Python appends `" subaccount:<vault>"` to
  the amount string (`"1.0 subaccount:0x1719..."`) and posts
  `vaultAddress: null`; TS accepts the same suffix form. Signatures are valid
  but `decode_action` rejects it (sub-accounts are out of scope).
* `expiresAfter` must be absent/null for user-signed actions (HL rule;
  enforced by `HlVerifier`). `vaultAddress` is not part of the user-signed
  payload and is ignored.

## 5. Typed decode (`decode_action`)

| wire type | `interfaces::Action` |
|---|---|
| order | `Order{orders, grouping}` (builder ignored; `{p}` grouping -> `Na`) |
| cancel | `Cancel{cancels: [(a, o)]}` |
| cancelByCloid | `CancelByCloid{cancels: [(asset, cloid)]}` |
| modify | `Modify{oid: Oid(u64) \| Cloid([u8;16]), order}` |
| batchModify | `BatchModify{modifies}` |
| scheduleCancel | `ScheduleCancel{time: Option}` (absent or null -> None) |
| updateLeverage | `UpdateLeverage{asset, is_cross, leverage}` |
| updateIsolatedMargin | `UpdateIsolatedMargin{asset, is_buy, ntli: Usd(raw)}` |
| usdSend / spotSend / withdraw3 | `{destination, (token,) amount: Usd, time}` |
| usdClassTransfer | `UsdClassTransfer{amount, to_perp, nonce}` (subaccount suffix -> Err) |
| approveAgent | `ApproveAgent{agent_address, agent_name: Option, nonce}` |
| setReferrer | `SetReferrer{code}` |
| noop | `Noop` |
| anything else | `Err("unsupported action type: ...")` |

Unknown extra keys are ignored (e.g. TS `f`, `a`, Python/TS `builder`);
missing required keys, wrong JSON types, negative sizes/amounts, out-of-range
integers and over-precise decimals are errors. Strings must be strings
(`"p": 1` is rejected).

## 6. Building requests (simulator)

`order_action`, `cancel_action`, `cancel_by_cloid_action`, `modify_action`,
`batch_modify_action`, `schedule_cancel_action`, `update_leverage_action`,
`update_isolated_margin_action`, `set_referrer_action`, `noop_action`,
`usd_send_action`, `spot_send_action`, `withdraw3_action`,
`usd_class_transfer_action`, `approve_agent_action`, `action_to_json(&Action)`
produce the Python SDK key order; then `signed_l1_request(..)` /
`signed_user_request(..)` (or `sign_*` + `build_exchange_request`) give a
postable body. Golden tests check the builders reproduce the Python vectors'
exact msgpack bytes.
