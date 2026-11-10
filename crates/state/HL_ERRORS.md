# Error strings produced by `state`

Constants live in `src/errors.rs`. "HL" = text used by Hyperliquid itself
(as far as known); "local" = our own text where HL has no equivalent or the
feature is out of scope. Per-order errors appear as `OrderStatusResp::Error`
inside an `ok` response; tx-level errors are `TxReceipt.result = Err(..)`
(HL `{"status":"err","response": msg}`).

## Per-order (order / batchModify statuses)
| text | source | when |
|---|---|---|
| `Order has invalid price.` | HL | `!MarketSpec::is_valid_px(limit_px)` (tick / 5 sig figs / decimals) |
| `Order has invalid size.` | HL | `!MarketSpec::is_valid_sz(sz)` (lot / zero) |
| `Invalid TP/SL price.` | HL | trigger_px fails `is_valid_px` |
| `Order must have minimum value of $10.` | HL | `limit_px * sz < 10 USD`, not reduce-only |
| `Insufficient margin to place order.` | HL | `check_order_margin` fails at admission, or the hook stops the taker on margin before any fill |
| `Reduce only order would increase position.` | HL | reduce-only with zero capacity (admission or first match) |
| `Post only order would have immediately matched, bbo was {bid}@{ask}` | HL | ALO crossing; missing side printed as `None` |
| `Order could not immediately match against any resting orders.` | HL | IOC (incl. market) with zero fill |
| `Duplicate cloid.` | local | user already has an open order (any market) with this cloid |
| `Invalid asset` | local (= clearinghouse `ERR_ASSET`) | asset index out of range |
| `Parent order failed.` | local | normalTpsl child when the parent errored |
| `TP/SL child order must be a trigger order.` | local | normalTpsl child that is a limit order |
| `TP/SL child must have the same asset as the parent order.` | local | |
| `positionTpsl orders must be trigger orders.` | local | |
| `Cannot modify canceled or filled order` | HL | batchModify entry whose target is not open |

## Cancel statuses
| `Order was never placed, already canceled, or filled.` | HL | cancel / cancelByCloid target not open for this user |

## Tx-level
| text | source | when |
|---|---|---|
| `Invalid signature.` | local | `recover_signer` failed (receipt user = 0x0, no effect) |
| `Failed to decode action: …` | local | `decode` failed |
| `User or API Wallet {signer} does not exist.` | HL | principal has no funds/positions/agents/meta (e.g. revoked agent) |
| `Invalid nonce: duplicate nonce` | HL | nonce already used by this signer |
| `Invalid nonce: nonce N must be greater than M` | local wording | 100 nonces stored and N <= smallest |
| `Invalid nonce: nonce N is too far in the past` / `… future` | local wording | outside (time − 2 d, time + 1 d) |
| `Invalid nonce: action time does not match request nonce.` | local | user-signed action `time`/`nonce` != tx nonce |
| `Expired` | spec | `expires_after < block time` (nonce not consumed) |
| `Vault actions are not supported.` | local | `vault_address` set (D6) |
| `Cannot modify canceled or filled order` | HL | single `modify` target missing (other modify errors are the order error text) |
| `Scheduled cancel time too early, must be at least 5 seconds after current time.` | HL | scheduleCancel < now + 5 s |
| `Invalid leverage value` | = clearinghouse `ERR_LEVERAGE` | 0 or > maxLeverage |
| `Invalid amount` | = clearinghouse `ERR_AMOUNT` | non-positive transfer amount |
| `Invalid token` | = clearinghouse `ERR_TOKEN` | spotSend token not `NAME:0x<tokenId>` of a genesis spot token |
| `Withdrawal amount must be greater than the withdrawal fee.` | local | withdraw3 amount <= `params.withdraw_fee` |
| `Agent address is already in use.` | local | agent already approved by another principal |
| `Cannot approve self as agent.` | local | |
| `Referrer already set.` | local | second setReferrer |
| clearinghouse `ExecError` text | engine | passed through unchanged, see below |

## Passed through from `clearinghouse` (its `ERR_*` constants)
`Insufficient margin to place order.` (fills), `Insufficient balance for
withdrawal`, `Insufficient balance for transfer`, `Insufficient balance for
token transfer`, `Invalid amount`, `Invalid asset`, `Invalid token`,
`Invalid leverage value`, `Cross margin is not allowed for this asset.`,
`Cannot switch leverage type with open position.`, `Insufficient margin to
decrease leverage.`, `No isolated position to update margin.`,
`Insufficient margin to add.`, `Insufficient margin to remove.`,
`Invalid user`, `Invalid fill`.
