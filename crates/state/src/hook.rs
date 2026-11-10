//! The state machine's `MatchHook`: STP, reduce-only, margin checks, fills.

use crate::views::resting_view;
use interfaces::*;

pub struct Hook<'a, C: Clearinghouse> {
    pub ch: &'a mut C,
    pub events: &'a mut Vec<Event>,
    pub next_tid: &'a mut Tid,
    pub asset: AssetId,
    pub time: TimestampMs,
    pub tx_hash: TxHash,
    pub taker_cloid: Option<Cloid>,
    pub liquidation: Option<LiquidationInfo>,
    /// Set when matching stopped because the taker lacked margin.
    pub taker_margin_stopped: bool,
    /// Set when matching stopped because a reduce-only taker had no capacity.
    pub taker_reduce_stopped: bool,
}

impl<C: Clearinghouse> MatchHook for Hook<'_, C> {
    fn on_match(&mut self, c: &CandidateFill<'_>) -> (MatchDecision, Sz) {
        let maker = c.maker;
        // 1. Self-trade prevention: cancel the resting (maker) order.
        if maker.user == c.taker_user {
            return (MatchDecision::CancelMaker(OrderLifecycle::SelfTradeCanceled), Sz::ZERO);
        }
        let mut sz = c.sz;
        // 2. Reduce-only, both sides.
        if maker.reduce_only {
            let cap = self.ch.reduce_only_capacity(&maker.user, self.asset, maker.side);
            if cap.0 <= 0 {
                return (MatchDecision::CancelMaker(OrderLifecycle::ReduceOnlyCanceled), Sz::ZERO);
            }
            sz = sz.min(cap);
        }
        if c.taker_reduce_only {
            let cap = self.ch.reduce_only_capacity(&c.taker_user, self.asset, c.taker_side);
            if cap.0 <= 0 {
                self.taker_reduce_stopped = true;
                return (MatchDecision::StopTaker, Sz::ZERO);
            }
            sz = sz.min(cap);
        }
        // 3. Maker margin re-check.
        if !maker.reduce_only {
            let mc = MarginCheck {
                user: maker.user,
                asset: self.asset,
                side: maker.side,
                px: c.px,
                sz,
                reduce_only: false,
            };
            if self.ch.check_order_margin(&mc).is_err() {
                return (MatchDecision::CancelMaker(OrderLifecycle::MarginCanceled), Sz::ZERO);
            }
        }
        // 4. Taker margin (skipped for liquidations and reduce-only).
        if self.liquidation.is_none() && !c.taker_reduce_only {
            let mc = MarginCheck {
                user: c.taker_user,
                asset: self.asset,
                side: c.taker_side,
                px: c.px,
                sz,
                reduce_only: false,
            };
            if self.ch.check_order_margin(&mc).is_err() {
                self.taker_margin_stopped = true;
                return (MatchDecision::StopTaker, Sz::ZERO);
            }
        }
        // 5. Apply.
        let fp = FillParams {
            asset: self.asset,
            px: c.px,
            sz,
            taker: c.taker_user,
            maker: maker.user,
            taker_side: c.taker_side,
            liquidation_of: self.liquidation.as_ref().map(|l| l.liquidated_user),
            time: self.time,
        };
        let r = match self.ch.apply_fill(&fp) {
            Ok(r) => r,
            Err(_) => {
                // Not expected after the checks above; never partially applied.
                self.taker_margin_stopped = true;
                return (MatchDecision::StopTaker, Sz::ZERO);
            }
        };
        let tid = *self.next_tid;
        *self.next_tid += 1;
        self.events.push(Event::Fill(FillEvent {
            asset: self.asset,
            tid,
            px: c.px,
            sz,
            time: self.time,
            tx_hash: self.tx_hash,
            taker: c.taker_user,
            maker: maker.user,
            taker_oid: c.taker_oid,
            maker_oid: maker.oid,
            taker_side: c.taker_side,
            taker_fee: r.taker_fee,
            maker_fee: r.maker_fee,
            taker_closed_pnl: r.taker_closed_pnl,
            maker_closed_pnl: r.maker_closed_pnl,
            taker_start_pos: r.taker_start_pos,
            maker_start_pos: r.maker_start_pos,
            taker_cloid: self.taker_cloid,
            maker_cloid: maker.cloid,
            liquidation: self.liquidation.clone(),
        }));
        if sz == maker.sz {
            let mut v = resting_view(maker);
            v.sz = Sz::ZERO;
            self.events.push(Event::OrderUpdate {
                user: maker.user,
                order: v,
                status: OrderLifecycle::Filled,
                time: self.time,
            });
        }
        (MatchDecision::Fill, sz)
    }
}
