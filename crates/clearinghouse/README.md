# clearinghouse

Cross / isolated-margin perpetual clearinghouse with Hyperliquid semantics.
Implements `interfaces::Clearinghouse`. All math is integer fixed point
(`interfaces::fixed`, i128 intermediates, 256-bit where needed in
`math::mul_div_wide`); every lossy operation names its rounding. No floats,
no HashMap iteration (the address→index `HashMap` is lookup-only), no clock.

```rust
let mut ch = clearinghouse::Clearinghouse::new(&genesis);
let flows_genesis = clearinghouse::Clearinghouse::genesis_total(&genesis); // ExternalFlows.genesis
let ch2 = clearinghouse::Clearinghouse::decode(&ch.encode())?;
```

Extra inherent API (beyond the trait): `withdraw_with_fee`, `withdrawable`,
`spot_balance`, `account`, `market_state`, `ledger`, `addresses`,
`negative_equity_accounts` (AC4 post-liquidation check), `adl_ranking`,
`liquidation_targets_full_scan` (reference O(n) scan), `all_entity_hashes`
(rebuild the state crate's entity map after `decode`), `genesis_total`.

## Cash accounting (why conservation is exact)

Every account has a **cross cash** bucket (HL cross `rawUsd`), each isolated
position an **isolated cash** bucket (HL isolated `rawUsd`), plus an
`iso_deficit` bucket (see liquidation). A fill of size `sz` at `px` moves a
single rounded notional `N = floor(px·sz)` from the buyer's bucket to the
seller's bucket — spot-like settlement. Fees move from the payer's cross cash
to the fee pool. So for every fill Σ(cash changes) = −(fees) exactly, and the
fee pool receives exactly the fees.

Position value is `szi·mark` and **equity = cash + Σ szi·mark** (cross) or
`iso_cash + szi·mark` (isolated). Unrealized PnL never needs to be "paid":
it's implicit in cash + value. Each position also keeps a signed cost basis
(`basis`, Σ notionals of the open size) used only for entry price, uPnL
display (`value − basis`) and `closedPnl` reporting:

* increase: `basis += sign·N`
* reduce `q` of `|s0|`: removed basis `r = basis·q/|s0|` (toward zero; all of
  it on a full close), `closedPnl = sign(s0)·N_close − r`, where `N_close =
  floor(N·q/|d|)` for flips and `N_open = N − N_close` opens the new side.

Conservation invariant (checked by `check_invariants`):

```
Σ cross cash + Σ isolated cash + Σ iso_deficit + Σ spot USDC + fee pool
  (+ insurance fund cash, which also holds funding dust) == flows.net()
```

A running total is updated by every mutation (O(1) check every call) and the
full sum is recomputed when there are ≤ 4096 accounts or every 64 heights.
Other checks: OI long == short per market (running and recomputed), funding
`paid == received + dust` per market, dust total, spot token supplies
(non-USDC tokens have no external flows), holder index, position layout.

`total_collateral()` returns the running total above; `insurance_fund()`
returns the insurance fund's equity (cash + value of positions it took over).

## Margin

| quantity | formula (rounding) |
|---|---|
| value | `szi·mark` (Floor) |
| \|ntl\| | `|szi|·mark` (Ceil) |
| initial margin | `ceil(|ntl| / leverage)` |
| maintenance | `ceil(|ntl| / (2·maxLeverage))` |
| cross equity | `cash + Σ_cross value` |
| withdrawable | `max(0, cross equity − Σ_cross IM)` (also limits usdSend and perp→spot) |
| isolated equity | `iso_cash + value` |

* Default leverage: `min(20, maxLeverage)`, cross (isolated for `only_isolated`).
* Fills: for each non-liquidated side whose trade is **risk-increasing**
  (|szi| grows or flips) the post-trade state must have
  `cross equity − cross IM >= 0`, else `"Insufficient margin to place order."`.
  Reducing trades are always allowed. `check_order_margin` runs the same
  computation (with fee = max(taker, maker) rate) so a pre-check that passes
  implies the fill-time check passes for the same price/size.
* Isolated open/increase: `T = ceil(N_open / leverage)` moves from cross cash
  into the isolated bucket (so isolated equity ≈ T); the fee is paid from
  cross cash. Partial reduce releases `floor(E·q/|s0|)` of the isolated
  equity `E` (if positive) back to cross, keeping equity per unit constant.
  Full close releases the whole bucket (a negative bucket is absorbed by
  cross cash for voluntary closes, or parked in `iso_deficit` for liquidation
  fills, so isolation holds; the deficit is paid by the insurance fund in
  `backstop_and_adl`).
* `update_leverage`: 1..=maxLeverage; cross refused on `only_isolated`;
  mode switch refused with an open position; decreasing leverage with an
  open position requires the new IM (cross: post-change cross available ≥ 0;
  isolated: isolated equity ≥ new IM); increasing always allowed.
* `update_isolated_margin`: add requires cross available ≥ delta; remove
  requires isolated equity − |delta| ≥ IM.

## Fees
`fee = ceil(N·rate)` for payers; negative maker rate (rebate) pays
`floor(N·|rate|)`. Fee pool may in principle go negative only if
|maker rebate| > taker fee (not the case for sane params).
`withdraw_with_fee(user, amount, fee)` debits `amount`, credits `fee` to the
fee pool: the state crate must add `amount − fee` to `flows.withdrawals`.

## Funding
* Premium sample (in `on_block_funding`, first block after each 5 s
  boundary of block time): `(mark − oracle)/oracle` (TowardZero). Mark =
  median(oracle, bid, ask) per ARCHITECTURE, which stands in for HL's impact
  prices.
* Settlement at the first block with `now / funding_interval_ms` greater than
  the last settled slot (settlement happens before that block's sample):
  `avg = Σ samples / n` (TowardZero; 0 if no samples),
  `rate = clamp(avg + clamp(interest − avg, ±0.0005), ±funding_cap)`.
* Payment per position `owed = szi·oracle·rate` (exact, 256-bit), rounded
  **up** (Ceil): payers pay `ceil(owed)`, receivers get `floor(|owed|)`.
  Since Σ szi = 0, Σ exact = 0, so dust = paid − received ∈ [0, #positions)
  µUSD and is credited to the insurance fund (`Ledger.funding_dust_total`,
  per-market `funding_dust`). One `Event::Funding` per position (usdc > 0 =
  received). Isolated positions pay from their bucket.

## Liquidation

**Liquidatable**: cross positions with cross equity < cross maintenance;
an isolated position with isolated equity < its maintenance; also negative
cross equity without positions or an `iso_deficit` (these produce targets
with empty `closes`). `liquidation_targets` sorts by address (cross target
first, then isolated targets by asset). `closes` = full position, limit
`mark·(1 ∓ liquidation_slippage)` rounded to a valid tick toward the mark.

**`backstop_and_adl(user)`** (after the market attempts): if the cross group
(or an isolated position) is *still* liquidatable it is closed entirely:

1. **Backstop**: the insurance fund (a pseudo-account `INSURANCE` committed
   under `StateKey::Insurance`) takes the positions at mark value and pays
   the deficit `D = max(0, −equity)` (`BadDebtCovered{by_insurance: true}`),
   provided its equity afterwards ≥ its maintenance margin. The user's bucket
   ends at exactly `max(equity, 0)`.
2. **ADL** otherwise: `D` is split over the positions ∝ |ntl| (last gets the
   remainder); position i closes for cash `value_i + d_i` (the bankruptcy
   price) against opposite-side holders ranked by `uPnL / margin × leverage`
   (margin = IM for cross, isolated equity for isolated) descending, ties by
   address; insurance (if it holds opposite size) is last resort. The cash is
   apportioned over chunks exactly (cumulative floor). `Event::Adl` per chunk
   and `BadDebtCovered{by_insurance: false}`. The user ends at exactly 0.
   Counterparties pushed below zero are resolved in the same call (worklist).
3. Negative cross equity without positions and isolated deficits are paid by
   the insurance fund (which may go negative only in this last case — there
   is nothing left to ADL).

Backstop / ADL fills are emitted as `Event::Fill` with `tid = 0`, zero oids
and tx hash (taker = liquidated user, maker = insurance / counterparty,
`liquidation.method = Backstop | Adl`); the state crate should assign tids.
After a full pass no account has negative equity (`negative_equity_accounts()`
is empty); accounts pushed below maintenance (not below zero) by ADL are
picked up by the next block's pass.

### Liquidation index
Each account is `Clean | Stale | AtRisk | Banded`. A banded account has
buffer `B = equity − MM − slack` (slack = 4·#positions + 1 µUSD covers
rounding) and sensitivity `S = Σ|ntl|(1+mmr)`; with `δ = B/S` it cannot
become liquidatable while every held market's mark stays in
`[mark·(1−δ), mark·(1+δ)]` (a relative move ≤ δ everywhere lowers
equity − MM by ≤ δ·S ≤ B). δ is the minimum over the cross group and each
isolated position. Bands live in per-market `BTreeSet<(edge, idx)>`;
`set_prices` range-queries the edges the new mark crossed and marks those
accounts stale. Mutations mark an account stale in O(1) (no index work on the
fill path); stale and at-risk accounts are re-evaluated (and re-banded) once
per `set_prices` call. `liquidation_targets` evaluates exactly only
`Stale ∪ AtRisk`, so results never depend on the index (proptests assert
equality with the full scan after every price move).

## State commitment
`take_dirty()` returns `(Account(addr), blake3(borsh(AccountState)))` for
touched accounts (ascending address), `(Market(a), blake3(borsh(MarketState)))`
and `(Insurance, blake3(borsh((insurance AccountState, Ledger))))`.
`encode()` = borsh of `ChSnapshot` (version, specs, params, spot tokens,
market states, all accounts sorted by address, ledger); `decode` rebuilds
the derived indices (holders, running total, liquidation index — all
accounts start `Stale`). After `decode`, use `all_entity_hashes()`.

## Views
`account_view`: HL clearinghouseState. `positionValue = |szi|·mark`,
`unrealizedPnl = value − basis`, `entryPx = |basis|/|szi|` (Floor),
`returnOnEquity = uPnL·leverage/|basis|`, `marginUsed` = IM (cross) or
isolated equity, liquidation price = mark − (E − MM)/(szi − |szi|·mmr)
(E, MM of the cross group or the isolated position; others held constant),
`cumFunding` positive = paid. `marginSummary` includes isolated equity /
raw usd; `crossMarginSummary` cross only; `withdrawable` as above.
`market_ctx`: funding = current estimate from samples so far, premium =
last sample, OI = long OI.

## Error strings
`Insufficient margin to place order.` (HL), others in `lib.rs` constants
(`ERR_*`), best-effort HL wording.

## Tests
* `cargo test -p clearinghouse` — unit tests (hand-computed numbers: fees,
  margin, weighted entry, partial close, flip, isolated allocation/release,
  leverage rules, transfers, funding interest/premium/dust, liquidation,
  backstop, bad debt, ADL ranking, isolated deficit, dirty tracking,
  snapshot round-trip, index vs full scan) and AC4 proptests
  (`tests/prop_invariants.rs`, 2×1000 cases: random op sequences among
  5–20 accounts and liquidation storms with tiny/zero insurance; after every
  step all invariants, atomicity of failed ops, index soundness, funding net
  zero; after every liquidation pass no negative equity; periodic
  encode/decode round trips continuing on the decoded copy).
* Benchmarks: `cargo test -p clearinghouse --release --test perf -- --ignored --nocapture`
  (`CH_BENCH_ACCOUNTS`, `CH_BENCH_MAKERS` env vars). On the shared dev box:
  apply_fill + check_order_margin ≈ 0.6 µs with 1k accounts × 10 positions,
  ≈ 1.6 µs with 100k accounts when makers are 50 hot accounts, ≈ 2.6 µs
  for uniformly random pairs over 100k accounts (cache-miss bound);
  per-block set_prices (12 markets) + liquidation_targets ≈ 1 µs vs ≈ 50 ms
  full scan for 100k accounts.
