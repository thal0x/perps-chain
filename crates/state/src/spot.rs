//! Spot trading (D25): one order book for spot pair 0 ("PURR/USDC", base
//! token 1, quote token 0 = USDC, asset id 10000), settled in spot balances.
//!
//! * No margin, leverage or positions. Reduce-only and trigger orders are
//!   rejected.
//! * Holds: a resting buy holds `ceil(px*sz) + fee reserve` quote, a resting
//!   sell holds `sz` base ([`order_hold`]). The per-user hold map is kept
//!   incrementally and is exactly `Σ order_hold(o)` over the user's resting
//!   orders (checked by [`SpotMarket::check`]). Free balance = balance − hold
//!   is what admission, spotSend and spot->perp transfers may use.
//! * A fill of `sz` at `px` moves `base(sz)` from seller to buyer and
//!   `q = floor(px*sz)` USDC from buyer to seller (the same amount on both
//!   sides, so conservation is exact). Fees are charged in quote (taker
//!   `ceil(q*taker_fee)`, maker `ceil(q*maker_fee)`, or a rebate
//!   `floor(q*|maker_fee|)` capped at the taker fee) and paid to
//!   [`SPOT_FEE_ACCOUNT`]'s spot USDC balance (the clearinghouse fee pool is
//!   perp cash and has no entry point for spot; see INTERFACE_REQUESTS.md).
//! * Every candidate fill re-checks that both parties stay `balance >= hold`
//!   on both tokens after the fill (the taker including the remainder it may
//!   rest); a maker that fails is canceled (`MarginCanceled`), a taker that
//!   fails stops matching.

use crate::views::resting_view;
use borsh::{BorshDeserialize, BorshSerialize};
use interfaces::*;
use std::collections::BTreeMap;

/// HL: spot asset id = 10000 + spot pair index.
pub const SPOT_ASSET_OFFSET: AssetId = 10_000;
/// The only tradable spot pair (index 0 = PURR/USDC).
pub const SPOT_ASSET: AssetId = SPOT_ASSET_OFFSET;
/// Spot trading fees (USDC) accumulate in this account's spot balance.
pub const SPOT_FEE_ACCOUNT: Address =
    Address([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xfe, 0xe5]);
/// Base/quote unit conversion: Sz is 1e-8, token amounts are 1e-6.
const SZ_PER_TOKEN_UNIT: i64 = (SZ_SCALE / USD_SCALE) as i64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpotPairSpec {
    /// Spot universe index (0).
    pub index: u32,
    /// 10000 + index.
    pub asset: AssetId,
    /// "PURR/USDC".
    pub name: String,
    pub base: u32,
    pub quote: u32,
    /// Base token szDecimals (PURR: 0).
    pub sz_decimals: u32,
}

impl SpotPairSpec {
    /// Pair 0 = token 1 / token 0 if the genesis has both tokens.
    pub fn from_genesis(g: &Genesis) -> Option<Self> {
        let quote = g.spot_tokens.iter().find(|t| t.index == 0)?;
        let base = g.spot_tokens.iter().find(|t| t.index == 1)?;
        assert!(base.sz_decimals <= USD_DECIMALS, "spot base szDecimals must be <= 6");
        Some(SpotPairSpec {
            index: 0,
            asset: SPOT_ASSET,
            name: format!("{}/{}", base.name, quote.name),
            base: base.index,
            quote: quote.index,
            sz_decimals: base.sz_decimals,
        })
    }
    /// HL spot rule: at most `8 - szDecimals` price decimals.
    pub fn max_px_decimals(&self) -> u32 {
        8u32.saturating_sub(self.sz_decimals)
    }
    /// HL spot price rule: 5 significant figures (integers always allowed),
    /// at most `8 - szDecimals` decimals.
    pub fn is_valid_px(&self, px: Px) -> bool {
        px.0 > 0 && px.has_at_most_decimals(self.max_px_decimals()) && (px_sig_figs(px) <= 5 || px.has_at_most_decimals(0))
    }
    pub fn is_valid_sz(&self, sz: Sz) -> bool {
        sz.0 > 0 && sz.has_at_most_decimals(self.sz_decimals)
    }
    /// Order size -> base token amount (exact: szDecimals <= 6).
    pub fn base_units(&self, sz: Sz) -> Usd {
        assert!(sz.0 % SZ_PER_TOKEN_UNIT == 0, "spot size below token precision");
        Usd(sz.0 / SZ_PER_TOKEN_UNIT)
    }
    /// Base token amount -> size units (startPosition in fills).
    pub fn sz_of_base(&self, u: Usd) -> Sz {
        Sz(u.0.checked_mul(SZ_PER_TOKEN_UNIT).expect("spot balance overflow"))
    }
    /// Synthetic market spec used only to construct/decode the book.
    pub fn market_spec(&self) -> MarketSpec {
        MarketSpec {
            asset: self.asset,
            name: self.name.clone(),
            sz_decimals: self.sz_decimals,
            max_leverage: 1,
            only_isolated: false,
            initial_px: Px::ONE,
        }
    }
}

/// Amount reserved by resting spot orders (token units, 1e-6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SpotHold {
    pub quote: Usd,
    pub base: Usd,
}

impl SpotHold {
    pub fn is_zero(&self) -> bool {
        self.quote.is_zero() && self.base.is_zero()
    }
    fn add(&mut self, o: SpotHold) {
        self.quote += o.quote;
        self.base += o.base;
    }
    fn sub(&mut self, o: SpotHold) {
        self.quote -= o.quote;
        self.base -= o.base;
        assert!(self.quote.0 >= 0 && self.base.0 >= 0, "negative spot hold");
    }
}

/// Fee rate used for the buy-side fee reserve: max(taker, maker, 0).
pub fn hold_fee_rate(p: &ChainParams) -> Rate {
    p.taker_fee.max(p.maker_fee).max(Rate::ZERO)
}

/// Hold of one resting order with remaining size `sz`.
/// Buy: `n + ceil(n * fee_rate)` quote with `n = ceil(px * sz)`; sell: `sz` base.
pub fn order_hold(pair: &SpotPairSpec, side: Side, px: Px, sz: Sz, fee_rate: Rate) -> SpotHold {
    if sz.0 == 0 {
        return SpotHold::default();
    }
    match side {
        Side::Bid => {
            let n = notional(px, sz, Rounding::Ceil);
            SpotHold { quote: n + usd_mul_rate(n, fee_rate, Rounding::Ceil), base: Usd::ZERO }
        }
        Side::Ask => SpotHold { quote: Usd::ZERO, base: pair.base_units(sz) },
    }
}

/// Signed fee for notional `q` at `rate`: positive rounds up (payer), a
/// rebate (negative rate) rounds toward zero.
pub fn spot_fee(q: Usd, rate: Rate) -> Usd {
    if rate.0 >= 0 {
        usd_mul_rate(q, rate, Rounding::Ceil)
    } else {
        usd_mul_rate(q, rate, Rounding::TowardZero)
    }
}

/// Committed spot-market state besides the book itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SpotCore {
    /// Per-user holds (nonzero entries only).
    pub holds: BTreeMap<Address, SpotHold>,
    /// Last trade price (markPx when the book is one-sided).
    pub last_px: Option<Px>,
}

pub struct SpotMarket<B> {
    pub pair: SpotPairSpec,
    pub book: B,
    pub core: SpotCore,
    pub dirty: bool,
}

impl<B: OrderBook> SpotMarket<B> {
    pub fn hold(&self, user: &Address) -> SpotHold {
        self.core.holds.get(user).copied().unwrap_or_default()
    }
    pub(crate) fn add_hold(&mut self, user: Address, h: SpotHold) {
        if h.is_zero() {
            return;
        }
        self.core.holds.entry(user).or_default().add(h);
    }
    pub(crate) fn release_hold(&mut self, user: Address, h: SpotHold) {
        release(&mut self.core.holds, user, h);
    }
    pub fn mid(&self) -> Option<Px> {
        match (self.book.best_bid(), self.book.best_ask()) {
            (Some(b), Some(a)) => Some(Px((b.0 + a.0) / 2)),
            _ => None,
        }
    }
    /// HL-style mark: median(last trade, bid, ask) when two-sided, else the
    /// last trade, else the mid of whatever side exists, else 1.0.
    pub fn mark(&self) -> Px {
        let (b, a) = (self.book.best_bid(), self.book.best_ask());
        match (b, a, self.core.last_px) {
            (Some(b), Some(a), Some(l)) => {
                let mut v = [b, a, l];
                v.sort();
                v[1]
            }
            (Some(b), Some(a), None) => Px((b.0 + a.0) / 2),
            (_, _, Some(l)) => l,
            (Some(x), None, None) | (None, Some(x), None) => x,
            (None, None, None) => Px::ONE,
        }
    }
    /// Holds map == Σ order_hold over the book (exact), and every resting
    /// order belongs to a user present in the map.
    pub fn check(&self, fee_rate: Rate) -> Result<(), String> {
        let mut n = 0usize;
        for (u, h) in &self.core.holds {
            if h.is_zero() || h.quote.0 < 0 || h.base.0 < 0 {
                return Err(format!("bad spot hold entry for {u}"));
            }
            let mut sum = SpotHold::default();
            for o in self.book.user_orders(u) {
                sum.add(order_hold(&self.pair, o.side, o.limit_px, o.sz, fee_rate));
                n += 1;
            }
            if sum != *h {
                return Err(format!("spot hold of {u} is {h:?}, orders sum to {sum:?}"));
            }
        }
        if n != self.book.len() {
            return Err(format!("spot book has {} orders, holds cover {n}", self.book.len()));
        }
        Ok(())
    }
}

fn release(holds: &mut BTreeMap<Address, SpotHold>, user: Address, h: SpotHold) {
    if h.is_zero() {
        return;
    }
    let e = holds.get_mut(&user).expect("released hold of user without holds");
    e.sub(h);
    if e.is_zero() {
        holds.remove(&user);
    }
}

/// Spot balance reader (engine-specific fast path via `Engines::spot_balance`).
pub type BalFn<C> = fn(&C, &Address, u32) -> Usd;

/// Match hook for the spot book.
pub struct SpotHook<'a, C: Clearinghouse> {
    pub ch: &'a mut C,
    pub bal: BalFn<C>,
    pub events: &'a mut Vec<Event>,
    pub next_tid: &'a mut Tid,
    pub pair: &'a SpotPairSpec,
    pub holds: &'a mut BTreeMap<Address, SpotHold>,
    pub last_px: &'a mut Option<Px>,
    pub taker_fee: Rate,
    pub maker_fee: Rate,
    pub hold_rate: Rate,
    pub time: TimestampMs,
    pub tx_hash: TxHash,
    pub taker_cloid: Option<Cloid>,
    /// The taker's limit price and remaining size (the part that may rest
    /// if the TIF is GTC).
    pub taker_px: Px,
    pub taker_remaining: Sz,
    pub taker_may_rest: bool,
    /// Matching stopped because the taker could not pay.
    pub taker_stopped: bool,
}

impl<C: Clearinghouse> SpotHook<'_, C> {
    fn hold_of(&self, u: &Address) -> SpotHold {
        self.holds.get(u).copied().unwrap_or_default()
    }
}

impl<C: Clearinghouse> MatchHook for SpotHook<'_, C> {
    fn on_match(&mut self, c: &CandidateFill<'_>) -> (MatchDecision, Sz) {
        let maker = c.maker;
        let pair = self.pair;
        let maker_order_hold = order_hold(pair, maker.side, maker.limit_px, maker.sz, self.hold_rate);
        // 1. Self-trade prevention: cancel the resting order.
        if maker.user == c.taker_user {
            release(self.holds, maker.user, maker_order_hold);
            return (MatchDecision::CancelMaker(OrderLifecycle::SelfTradeCanceled), Sz::ZERO);
        }
        let sz = c.sz;
        let px = c.px;
        let q = notional(px, sz, Rounding::Floor);
        let b = pair.base_units(sz);
        let taker_fee = spot_fee(q, self.taker_fee);
        let mut maker_fee = spot_fee(q, self.maker_fee);
        if maker_fee.0 < 0 && -maker_fee.0 > taker_fee.0 {
            maker_fee = Usd(-taker_fee.0);
        }
        let taker_buys = c.taker_side == Side::Bid;
        // Post-fill balances (quote, base) of both parties.
        let (bq, bb) = (pair.quote, pair.base);
        let tq0 = (self.bal)(self.ch, &c.taker_user, bq);
        let tb0 = (self.bal)(self.ch, &c.taker_user, bb);
        let mq0 = (self.bal)(self.ch, &maker.user, bq);
        let mb0 = (self.bal)(self.ch, &maker.user, bb);
        let (tq1, tb1, mq1, mb1) = if taker_buys {
            (tq0 - q - taker_fee, tb0 + b, mq0 + q - maker_fee, mb0 - b)
        } else {
            (tq0 + q - taker_fee, tb0 - b, mq0 - q - maker_fee, mb0 + b)
        };
        // 2. Maker must stay covered (its hold shrinks with the fill).
        let mut maker_hold1 = self.hold_of(&maker.user);
        maker_hold1.sub(maker_order_hold);
        maker_hold1.add(order_hold(pair, maker.side, maker.limit_px, maker.sz - sz, self.hold_rate));
        if mq1 < maker_hold1.quote || mb1 < maker_hold1.base {
            release(self.holds, maker.user, maker_order_hold);
            return (MatchDecision::CancelMaker(OrderLifecycle::MarginCanceled), Sz::ZERO);
        }
        // 3. Taker must stay covered, including what it may still rest.
        let mut taker_hold1 = self.hold_of(&c.taker_user);
        if self.taker_may_rest {
            taker_hold1.add(order_hold(pair, c.taker_side, self.taker_px, self.taker_remaining - sz, self.hold_rate));
        }
        if tq1 < taker_hold1.quote || tb1 < taker_hold1.base {
            self.taker_stopped = true;
            return (MatchDecision::StopTaker, Sz::ZERO);
        }
        // 4. Settle (all transfers pre-checked above).
        let (buyer, seller) = if taker_buys { (c.taker_user, maker.user) } else { (maker.user, c.taker_user) };
        let send = |ch: &mut C, from: Address, to: Address, token: u32, amt: Usd| {
            if amt.0 > 0 {
                ch.spot_send(from, to, token, amt).expect("pre-checked spot settlement transfer");
            }
        };
        send(self.ch, seller, buyer, bb, b);
        send(self.ch, buyer, seller, bq, q);
        send(self.ch, c.taker_user, SPOT_FEE_ACCOUNT, bq, taker_fee);
        if maker_fee.0 > 0 {
            send(self.ch, maker.user, SPOT_FEE_ACCOUNT, bq, maker_fee);
        } else {
            send(self.ch, SPOT_FEE_ACCOUNT, maker.user, bq, Usd(-maker_fee.0));
        }
        // Maker hold follows its remaining size.
        let new_maker_hold = order_hold(pair, maker.side, maker.limit_px, maker.sz - sz, self.hold_rate);
        release(self.holds, maker.user, maker_order_hold);
        if !new_maker_hold.is_zero() {
            self.holds.entry(maker.user).or_default().add(new_maker_hold);
        }
        *self.last_px = Some(px);
        self.taker_remaining -= sz;
        let tid = *self.next_tid;
        *self.next_tid += 1;
        self.events.push(Event::Fill(FillEvent {
            asset: pair.asset,
            tid,
            px,
            sz,
            time: self.time,
            tx_hash: self.tx_hash,
            taker: c.taker_user,
            maker: maker.user,
            taker_oid: c.taker_oid,
            maker_oid: maker.oid,
            taker_side: c.taker_side,
            taker_fee,
            maker_fee,
            taker_closed_pnl: Usd::ZERO,
            maker_closed_pnl: Usd::ZERO,
            // Spot "startPosition" = base balance before the fill.
            taker_start_pos: pair.sz_of_base(tb0),
            maker_start_pos: pair.sz_of_base(mb0),
            taker_cloid: self.taker_cloid,
            maker_cloid: maker.cloid,
            liquidation: None,
        }));
        if sz == maker.sz {
            let mut v = resting_view(maker);
            v.sz = Sz::ZERO;
            self.events.push(Event::OrderUpdate { user: maker.user, order: v, status: OrderLifecycle::Filled, time: self.time });
        }
        (MatchDecision::Fill, sz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> SpotPairSpec {
        SpotPairSpec { index: 0, asset: SPOT_ASSET, name: "PURR/USDC".into(), base: 1, quote: 0, sz_decimals: 0 }
    }

    #[test]
    fn px_rules() {
        let p = pair();
        assert!(p.is_valid_px(Px::parse("0.5").unwrap()));
        assert!(p.is_valid_px(Px::parse("0.00012345").unwrap()));
        assert!(!p.is_valid_px(Px::parse("0.000123456").unwrap_or(Px(1))) || Px::parse("0.000123456").is_err());
        assert!(!p.is_valid_px(Px::parse("1.23456").unwrap()));
        assert!(p.is_valid_px(Px::parse("123456").unwrap()));
        assert!(p.is_valid_sz(Sz::parse("24").unwrap()));
        assert!(!p.is_valid_sz(Sz::parse("24.5").unwrap()));
        let p2 = SpotPairSpec { sz_decimals: 2, ..pair() };
        assert!(p2.is_valid_px(Px::parse("0.012345").unwrap()));
        assert!(!p2.is_valid_px(Px::parse("0.0012345").unwrap()));
    }

    #[test]
    fn holds_and_fees() {
        let p = pair();
        let r = Rate::from_raw(350_000_000); // 0.035%
        let h = order_hold(&p, Side::Bid, Px::parse("0.5").unwrap(), Sz::parse("24").unwrap(), r);
        assert_eq!(h.quote, Usd::parse("12.0042").unwrap());
        let h = order_hold(&p, Side::Ask, Px::parse("0.5").unwrap(), Sz::parse("24").unwrap(), r);
        assert_eq!(h, SpotHold { quote: Usd::ZERO, base: Usd::parse("24").unwrap() });
        assert_eq!(spot_fee(Usd::parse("1").unwrap(), r), Usd::parse("0.00035").unwrap());
        assert_eq!(spot_fee(Usd::parse("0.001").unwrap(), r), Usd::from_raw(1));
        assert_eq!(spot_fee(Usd::parse("0.001").unwrap(), Rate::from_raw(-350_000_000)), Usd::ZERO);
    }
}
