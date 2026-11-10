//! Read-only views (HL clearinghouseState / assetCtxs) and invariant checks.

use crate::math::*;
use crate::*;

impl Clearinghouse {
    /// HL-style liquidation price estimate: the mark of `p.asset` at which
    /// (equity − maintenance) of the position's margin pool reaches zero,
    /// all other marks held constant:
    /// `px = mark − (E − MM) / (szi − |szi|·mmr)`. None if <= 0.
    pub(crate) fn liq_px(&self, p: &Position, e_minus_mm: i128) -> Option<Px> {
        let s = p.szi.raw();
        if s == 0 {
            return None;
        }
        let mmr = self.mmr[p.asset as usize].raw();
        let den = s * RATE_SCALE - s.abs() * mmr; // scale 1e8 * 1e12
        if den == 0 {
            return None;
        }
        // (E−MM)[1e-6] / (den[1e-20]) -> px[1e-8]: × 1e22
        let shift = mul_div_wide(e_minus_mm, 10i128.pow(22), den, Rounding::TowardZero);
        let px = self.mark(p.asset).raw() - shift;
        if px <= 0 {
            None
        } else {
            Some(Px::from_raw(px.min(u64::MAX as i128)))
        }
    }

    pub(crate) fn account_view_impl(&self, user: &Address) -> AccountView {
        let empty = AccountState::new(*user);
        let st = self.account(user).unwrap_or(&empty);
        let cs = self.cross_sum(st, None, None, 0);
        let mut positions = Vec::with_capacity(st.positions.len());
        let mut av = cs.equity;
        let mut ntl_total = cs.ntl;
        let mut raw = st.cash.raw();
        let mut used = cs.im;
        for p in &st.positions {
            let lev = self.lev_in(st, p.asset);
            let value = self.value(p.asset, p.szi);
            let upnl = value - p.basis.raw();
            let ntl = self.ntl_abs(p.asset, p.szi);
            let pos_value = notional(self.mark(p.asset), p.szi.abs(), Rounding::Floor);
            let (margin_used, liq) = if lev.is_cross() {
                let im = div_round(ntl, lev.value() as i128, Rounding::Ceil);
                (im, self.liq_px(p, cs.equity - cs.mm))
            } else {
                let eq = self.iso_equity(p);
                av += eq;
                ntl_total += ntl;
                raw += p.iso_cash.raw();
                used += eq;
                (eq, self.liq_px(p, eq - self.mm_of(p.asset, ntl)))
            };
            // HL returnOnEquity = uPnL / (|entry ntl| / leverage)
            let roe = if p.basis.is_zero() {
                Rate::ZERO
            } else {
                Rate::from_raw(mul_div_wide(
                    upnl * lev.value() as i128,
                    RATE_SCALE,
                    p.basis.raw().abs(),
                    Rounding::TowardZero,
                ))
            };
            let all_time = st.setting(p.asset).map(|s| s.cum_funding_all_time).unwrap_or(Usd::ZERO);
            positions.push(PositionView {
                asset: p.asset,
                szi: p.szi,
                entry_px: px_from_ntl(p.basis, p.szi, Rounding::Floor),
                position_value: pos_value,
                unrealized_pnl: Usd::from_raw(upnl),
                return_on_equity: roe,
                leverage: lev,
                margin_used: Usd::from_raw(margin_used),
                liquidation_px: liq,
                max_leverage: self.specs[p.asset as usize].max_leverage,
                cum_funding_all_time: all_time,
                cum_funding_since_open: p.cum_funding_since_open,
            });
        }
        AccountView {
            margin_summary: MarginSummary {
                account_value: Usd::from_raw(av),
                total_ntl_pos: Usd::from_raw(ntl_total),
                total_raw_usd: Usd::from_raw(raw),
                total_margin_used: Usd::from_raw(used),
            },
            cross_margin_summary: MarginSummary {
                account_value: Usd::from_raw(cs.equity),
                total_ntl_pos: Usd::from_raw(cs.ntl),
                total_raw_usd: st.cash,
                total_margin_used: Usd::from_raw(cs.im),
            },
            cross_maintenance_margin_used: Usd::from_raw(cs.mm),
            withdrawable: Usd::from_raw((cs.equity - cs.im).max(0)),
            positions,
            spot_balances: st.spot.iter().filter(|(_, a)| !a.is_zero()).copied().collect(),
        }
    }

    pub(crate) fn market_ctx_impl(&self, asset: AssetId) -> MarketCtx {
        let m = &self.markets[asset as usize];
        MarketCtx {
            asset,
            oracle_px: m.oracle_px,
            mark_px: m.mark_px,
            mid_px: m.mid_px,
            funding: self.funding_rate_from(m),
            premium: m.last_premium,
            open_interest: m.oi_long,
            day_ntl_vlm: m.day_ntl_vlm,
            prev_day_px: m.prev_day_px,
        }
    }

    pub(crate) fn check_invariants_impl(&self, flows: &ExternalFlows, height: Height) -> Result<(), InvariantViolation> {
        let fail = |what: String| Err(InvariantViolation { height, what });
        // 1. conservation (running total, O(1))
        if self.total != flows.net().raw() {
            return fail(format!(
                "conservation: clearinghouse holds {} but external flows net {}",
                Usd::from_raw(self.total),
                flows.net()
            ));
        }
        // open interest long == short (O(markets))
        for (a, m) in self.markets.iter().enumerate() {
            if m.oi_long != m.oi_short {
                return fail(format!("open interest mismatch on {a}: long {:?} short {:?}", m.oi_long, m.oi_short));
            }
            // 2. funding nets to zero including dust
            if m.funding_paid != m.funding_received + m.funding_dust || m.funding_dust.0 < 0 {
                return fail(format!("funding does not net to zero on {a}"));
            }
        }
        let dust: Usd = self.markets.iter().fold(Usd::ZERO, |s, m| s + m.funding_dust);
        if dust != self.ledger.funding_dust_total {
            return fail("funding dust total mismatch".into());
        }
        if self.accts.len() > FULL_CHECK_MAX_ACCOUNTS && height % FULL_CHECK_EVERY != 0 {
            return Ok(());
        }
        // full recomputation
        let mut sum = self.ledger.fee_pool.raw();
        let mut long = vec![0i128; self.markets.len()];
        let mut short = vec![0i128; self.markets.len()];
        let mut nholders = vec![0usize; self.markets.len()];
        let mut supply = vec![0i128; self.spot_tokens.len()];
        for st in &self.accts {
            sum += st.cash.raw() + st.iso_deficit.raw();
            if st.iso_deficit.0 > 0 {
                return fail(format!("positive isolated deficit for {}", st.address));
            }
            for (t, amt) in &st.spot {
                if amt.0 <= 0 || *t as usize >= supply.len() {
                    return fail(format!("bad spot balance for {}", st.address));
                }
                supply[*t as usize] += amt.raw();
            }
            let mut prev: Option<AssetId> = None;
            for p in &st.positions {
                if p.szi.is_zero() || prev.is_some_and(|x| x >= p.asset) {
                    return fail(format!("malformed positions for {}", st.address));
                }
                prev = Some(p.asset);
                let lev = self.lev_in(st, p.asset);
                if lev.is_cross() && !p.iso_cash.is_zero() {
                    return fail(format!("cross position with isolated cash for {}", st.address));
                }
                sum += p.iso_cash.raw();
                let a = p.asset as usize;
                if p.szi.0 > 0 {
                    long[a] += p.szi.raw();
                } else {
                    short[a] -= p.szi.raw();
                }
                nholders[a] += 1;
                if !self.holders[a].contains(&st.address) {
                    return fail(format!("holder index missing {}", st.address));
                }
            }
        }
        if !supply.is_empty() {
            sum += supply[0];
        }
        if sum != self.total {
            return fail(format!("running total {} != recomputed {}", Usd::from_raw(self.total), Usd::from_raw(sum)));
        }
        for t in 1..supply.len() {
            if self.ledger.spot_supply.get(t).map(|u| u.raw()).unwrap_or(0) != supply[t] {
                return fail(format!("spot token {t} supply changed"));
            }
        }
        for (a, m) in self.markets.iter().enumerate() {
            if long[a] != short[a] || long[a] != m.oi_long.raw() || short[a] != m.oi_short.raw() {
                return fail(format!("open interest recompute mismatch on {a}"));
            }
            if nholders[a] != self.holders[a].len() {
                return fail(format!("holder index size mismatch on {a}"));
            }
        }
        Ok(())
    }
}
