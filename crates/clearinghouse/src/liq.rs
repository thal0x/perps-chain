//! Liquidation index, liquidation targets, insurance backstop and ADL.
//!
//! ## Index (see README "Liquidation index")
//! Every account (except the insurance fund) is in one of four states:
//! `Clean` (nothing open), `Stale` (mutated since last evaluation),
//! `AtRisk` (liquidatable / indebted at last evaluation) or `Banded`.
//! A banded account with safety buffer `B = equity − maintenance − slack`
//! and sensitivity `S = Σ |ntl_i|·(1 + mmr_i)` is provably not liquidatable
//! while every held market's mark stays within `mark_ref·(1 ± δ)`,
//! `δ = B / S`, because a relative move of at most δ in every market lowers
//! equity − maintenance by at most `δ·S <= B`. Bands are stored in per-market
//! ordered sets keyed by the band edge; `set_prices` pulls only the accounts
//! whose band the new mark leaves. Account mutations mark the account
//! `Stale` in O(1); stale accounts are re-banded once per `set_prices`
//! call (once per block in practice). `liquidation_targets` evaluates only
//! `Stale ∪ AtRisk` accounts exactly, so its result never depends on the
//! index (only its cost does).

use crate::math::*;
use crate::*;

impl Clearinghouse {
    pub(crate) fn on_mark_moved(&mut self, asset: AssetId) {
        let mark = self.mark(asset);
        let a = asset as usize;
        let mut hit: Vec<u32> = Vec::new();
        // lo > mark  or  hi < mark  => left the band
        if let Some(next) = mark.0.checked_add(1) {
            hit.extend(self.band_lo[a].range((Px(next), 0)..).map(|(_, i)| *i));
        }
        hit.extend(self.band_hi[a].range(..(mark, 0)).map(|(_, i)| *i));
        for i in hit {
            let m = &mut self.meta[i as usize];
            if m.liq != LiqState::Stale {
                m.liq = LiqState::Stale;
                self.stale.push(i);
            }
        }
    }

    /// Re-evaluate all stale and at-risk accounts at current marks.
    pub(crate) fn refresh_pending(&mut self) {
        let mut todo = std::mem::take(&mut self.stale);
        todo.extend(self.at_risk.iter().copied());
        todo.sort_unstable();
        todo.dedup();
        for i in todo {
            self.refresh(i as usize);
        }
    }

    fn remove_bands(&mut self, i: usize) {
        let bands = std::mem::take(&mut self.meta[i].bands);
        for (a, lo, hi) in bands {
            self.band_lo[a as usize].remove(&(lo, i as u32));
            self.band_hi[a as usize].remove(&(hi, i as u32));
        }
    }

    /// Exact liquidation status of an account.
    pub(crate) fn is_liquidatable(&self, st: &AccountState) -> bool {
        let cs = self.cross_sum(st, None, None, 0);
        if (cs.n > 0 && cs.equity < cs.mm) || cs.equity < 0 || st.iso_deficit.0 < 0 {
            return true;
        }
        st.positions.iter().any(|p| {
            !self.lev_in(st, p.asset).is_cross() && self.iso_equity(p) < self.mm_of(p.asset, self.ntl_abs(p.asset, p.szi))
        })
    }

    fn refresh(&mut self, i: usize) {
        if i == 0 {
            return;
        }
        self.remove_bands(i);
        let st = &self.accts[i];
        if self.is_liquidatable(st) {
            self.meta[i].liq = LiqState::AtRisk;
            self.at_risk.insert(i as u32);
            return;
        }
        self.at_risk.remove(&(i as u32));
        if st.positions.is_empty() {
            self.meta[i].liq = LiqState::Clean;
            return;
        }
        let slack = 4 * st.positions.len() as i128 + 1;
        let one = RATE_SCALE;
        let mut delta = one; // cap at 100%
        let cs = self.cross_sum(st, None, None, 0);
        if cs.n > 0 {
            let b = cs.equity - cs.mm - slack;
            delta = delta.min(if b <= 0 || cs.sens == 0 { 0 } else { mul_div_wide(b, one, cs.sens, Rounding::Floor) });
        }
        for p in &st.positions {
            if self.lev_in(st, p.asset).is_cross() {
                continue;
            }
            let ntl = self.ntl_abs(p.asset, p.szi);
            let b = self.iso_equity(p) - self.mm_of(p.asset, ntl) - slack;
            let s = self.sens_of(p.asset, ntl);
            delta = delta.min(if b <= 0 || s == 0 { 0 } else { mul_div_wide(b, one, s, Rounding::Floor) });
        }
        let mut bands = Vec::with_capacity(st.positions.len());
        for p in &st.positions {
            let mk = self.mark(p.asset).raw();
            let lo = mul_div_wide(mk, one - delta, one, Rounding::Ceil);
            let hi = mul_div_wide(mk, one + delta, one, Rounding::Floor).min(u64::MAX as i128);
            bands.push((p.asset, Px::from_raw(lo), Px::from_raw(hi)));
        }
        for (a, lo, hi) in &bands {
            self.band_lo[*a as usize].insert((*lo, i as u32));
            self.band_hi[*a as usize].insert((*hi, i as u32));
        }
        self.meta[i].bands = bands;
        self.meta[i].liq = LiqState::Banded;
    }

    /// Limit price for a liquidation close of `sz` (signed trade size).
    fn liq_limit(&self, asset: AssetId, trade: Sz) -> Px {
        let mark = self.mark(asset);
        let slip = self.params.liquidation_slippage.raw().clamp(0, RATE_SCALE - 1);
        let spec = &self.specs[asset as usize];
        if trade.0 > 0 {
            // buying back a short: pay at most mark·(1+slip), rounded down to a valid px
            let px = Px::from_raw(mul_div_wide(mark.raw(), RATE_SCALE + slip, RATE_SCALE, Rounding::Floor));
            round_px_valid(spec, px, false)
        } else {
            let px = Px::from_raw(mul_div_wide(mark.raw(), RATE_SCALE - slip, RATE_SCALE, Rounding::Ceil).max(1));
            round_px_valid(spec, px, true)
        }
    }

    pub(crate) fn targets_for(&self, st: &AccountState) -> Vec<LiquidationTarget> {
        let mut out = Vec::new();
        let cs = self.cross_sum(st, None, None, 0);
        if (cs.n > 0 && cs.equity < cs.mm) || (cs.n == 0 && cs.equity < 0) || st.iso_deficit.0 < 0 {
            let closes = if cs.n > 0 && cs.equity < cs.mm {
                st.positions
                    .iter()
                    .filter(|p| self.lev_in(st, p.asset).is_cross())
                    .map(|p| (p.asset, -p.szi, self.liq_limit(p.asset, -p.szi)))
                    .collect()
            } else {
                Vec::new()
            };
            out.push(LiquidationTarget { user: st.address, closes, isolated_asset: None });
        }
        for p in &st.positions {
            if self.lev_in(st, p.asset).is_cross() {
                continue;
            }
            if self.iso_equity(p) < self.mm_of(p.asset, self.ntl_abs(p.asset, p.szi)) {
                out.push(LiquidationTarget {
                    user: st.address,
                    closes: vec![(p.asset, -p.szi, self.liq_limit(p.asset, -p.szi))],
                    isolated_asset: Some(p.asset),
                });
            }
        }
        out
    }

    pub(crate) fn liquidation_targets_impl(&self) -> Vec<LiquidationTarget> {
        let mut cand: Vec<&AccountState> = self
            .stale
            .iter()
            .chain(self.at_risk.iter())
            .filter(|i| **i != 0)
            .map(|i| &self.accts[*i as usize])
            .collect();
        cand.sort_by_key(|a| a.address);
        cand.dedup_by(|a, b| a.address == b.address);
        let mut out = Vec::new();
        for st in cand {
            out.extend(self.targets_for(st));
        }
        out
    }

    /// Exact O(all accounts) scan (reference implementation for tests).
    pub fn liquidation_targets_full_scan(&self) -> Vec<LiquidationTarget> {
        let mut v: Vec<&AccountState> = self.accts[1..].iter().collect();
        v.sort_by_key(|a| a.address);
        v.into_iter().flat_map(|st| self.targets_for(st)).collect()
    }

    // ------------------------------------------------ backstop / ADL ---

    fn cover_by_insurance(&mut self, i: usize, asset: AssetId, amt: i128, iso_asset: Option<AssetId>, deficit: bool, ev: &mut Vec<Event>) {
        if amt <= 0 {
            return;
        }
        let user = self.accts[i].address;
        // insurance cash -> user's bucket (zero-sum)
        self.accts[0].cash = Usd::from_raw(self.accts[0].cash.raw() - amt);
        let st = &mut self.accts[i];
        if deficit {
            st.iso_deficit = Usd::from_raw(st.iso_deficit.raw() + amt);
        } else if let Some(a) = iso_asset {
            let k = st.positions.binary_search_by_key(&a, |p| p.asset).expect("iso pos");
            st.positions[k].iso_cash += Usd::from_raw(amt);
        } else {
            st.cash = Usd::from_raw(st.cash.raw() + amt);
        }
        self.touch(0);
        self.touch(i);
        self.ledger.bad_debt_insurance += Usd::from_raw(amt);
        ev.push(Event::BadDebtCovered { asset, user, amount: Usd::from_raw(amt), by_insurance: true });
    }

    /// Resolve one account after market liquidation (see trait docs).
    pub(crate) fn resolve(&mut self, user: Address, now: TimestampMs, ev: &mut Vec<Event>, work: &mut VecDeque<Address>) {
        if user == INSURANCE {
            return;
        }
        let Some(i) = self.idx(&user) else { return };
        // cross
        let st = &self.accts[i];
        let cs = self.cross_sum(st, None, None, 0);
        if cs.n > 0 && cs.equity < cs.mm {
            let assets: Vec<AssetId> =
                st.positions.iter().filter(|p| self.lev_in(st, p.asset).is_cross()).map(|p| p.asset).collect();
            self.close_group(i, &assets, None, now, ev, work);
        } else if cs.n == 0 && cs.equity < 0 {
            self.cover_by_insurance(i, 0, -cs.equity, None, false, ev);
        }
        // isolated
        let st = &self.accts[i];
        let isos: Vec<AssetId> = st
            .positions
            .iter()
            .filter(|p| {
                !self.lev_in(st, p.asset).is_cross()
                    && self.iso_equity(p) < self.mm_of(p.asset, self.ntl_abs(p.asset, p.szi))
            })
            .map(|p| p.asset)
            .collect();
        for a in isos {
            self.close_group(i, &[a], Some(a), now, ev, work);
        }
        // isolated deficits from market liquidation below bankruptcy
        let d = self.accts[i].iso_deficit.raw();
        if d < 0 {
            self.cover_by_insurance(i, 0, -d, None, true, ev);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn fill_event(
        &self,
        asset: AssetId,
        liquidated: Address,
        cp: Address,
        delta_from: Sz,
        amount: i128,
        r: (Usd, Usd, Sz, Sz),
        method: LiquidationMethod,
        now: TimestampMs,
    ) -> Event {
        Event::Fill(FillEvent {
            asset,
            tid: 0,
            px: px_from_ntl(Usd::from_raw(amount), delta_from, Rounding::Floor),
            sz: delta_from.abs(),
            time: now,
            tx_hash: Hash32::ZERO,
            taker: liquidated,
            maker: cp,
            taker_oid: 0,
            maker_oid: 0,
            taker_side: if delta_from.0 > 0 { Side::Bid } else { Side::Ask },
            taker_fee: Usd::ZERO,
            maker_fee: Usd::ZERO,
            taker_closed_pnl: r.0,
            maker_closed_pnl: r.1,
            taker_start_pos: r.2,
            maker_start_pos: r.3,
            taker_cloid: None,
            maker_cloid: None,
            liquidation: Some(LiquidationInfo { liquidated_user: liquidated, mark_px: self.mark(asset), method }),
        })
    }

    /// Close `assets` of account `i` (all cross positions, or the single
    /// isolated position `iso`): insurance backstop at mark if the fund can
    /// absorb it, else ADL at bankruptcy prices.
    fn close_group(
        &mut self,
        i: usize,
        assets: &[AssetId],
        iso: Option<AssetId>,
        now: TimestampMs,
        ev: &mut Vec<Event>,
        work: &mut VecDeque<Address>,
    ) {
        if assets.is_empty() {
            return;
        }
        let user = self.accts[i].address;
        let st = &self.accts[i];
        let equity = match iso {
            Some(a) => self.iso_equity(st.pos(a).expect("iso pos")),
            None => self.cross_sum(st, None, None, 0).equity,
        };
        let deficit = (-equity).max(0);
        let pos: Vec<(AssetId, Sz)> = assets.iter().map(|a| (*a, st.szi(*a))).collect();

        // --- backstop feasibility: insurance equity after takeover and
        // covering the deficit must stay >= its maintenance requirement.
        let ins = &self.accts[0];
        let mut ins_cash = ins.cash.raw() - deficit;
        let mut ins_pos: Vec<(AssetId, Sz)> = ins.positions.iter().map(|p| (p.asset, p.szi)).collect();
        for (a, s) in &pos {
            ins_cash -= self.value(*a, *s);
            match ins_pos.iter_mut().find(|x| x.0 == *a) {
                Some(x) => x.1 += *s,
                None => ins_pos.push((*a, *s)),
            }
        }
        let mut ins_eq = ins_cash;
        let mut ins_mm = 0i128;
        for (a, s) in &ins_pos {
            ins_eq += self.value(*a, *s);
            ins_mm += self.mm_of(*a, self.ntl_abs(*a, *s));
        }
        let first_asset = assets[0];
        if ins_eq >= ins_mm {
            // cover the deficit first so the user's bucket ends at exactly 0
            self.cover_by_insurance(i, first_asset, deficit, iso, false, ev);
            for (a, s) in pos {
                let amount = self.value(a, s);
                let r = self.transfer_position(a, user, INSURANCE, -s, amount);
                ev.push(self.fill_event(a, user, INSURANCE, -s, amount, r, LiquidationMethod::Backstop, now));
            }
            return;
        }

        // --- ADL at bankruptcy prices: distribute the deficit over the
        // positions proportionally to notional (last takes the remainder).
        let ntls: Vec<i128> = pos.iter().map(|(a, s)| self.ntl_abs(*a, *s)).collect();
        let total_ntl: i128 = ntls.iter().sum();
        let mut left = deficit;
        for (k, (a, s)) in pos.iter().enumerate() {
            let d_k = if k + 1 == pos.len() || total_ntl == 0 {
                left
            } else {
                mul_div_wide(deficit, ntls[k], total_ntl, Rounding::Floor)
            };
            left -= d_k;
            // user's cash change for closing the whole position
            let amount = self.value(*a, *s) + d_k;
            self.adl_position(user, *a, *s, amount, now, ev, work);
        }
        if deficit > 0 {
            self.ledger.bad_debt_adl += Usd::from_raw(deficit);
            self.dirty_insurance = true;
            ev.push(Event::BadDebtCovered { asset: first_asset, user, amount: Usd::from_raw(deficit), by_insurance: false });
        }
    }

    /// ADL ranking of opposite-side holders: score = uPnL / margin ×
    /// leverage, descending, ties by address ascending; the insurance fund
    /// (if it holds an opposite position) is the counterparty of last resort.
    pub fn adl_ranking(&self, asset: AssetId, user: &Address, user_long: bool) -> Vec<Address> {
        let mut v: Vec<(i128, Address)> = Vec::new();
        let mut ins = false;
        for a in &self.holders[asset as usize] {
            if a == user {
                continue;
            }
            let st = &self.accts[self.idx(a).expect("holder")];
            let p = st.pos(asset).expect("holder pos");
            if (p.szi.0 > 0) == user_long {
                continue;
            }
            if *a == INSURANCE {
                ins = true;
                continue;
            }
            let lev = self.lev_in(st, asset);
            let upnl = self.value(asset, p.szi) - p.basis.raw();
            let margin = if lev.is_cross() {
                div_round(self.ntl_abs(asset, p.szi), lev.value() as i128, Rounding::Ceil)
            } else {
                self.iso_equity(p)
            }
            .max(1);
            let score = mul_div_wide(upnl * lev.value() as i128, RATE_SCALE, margin, Rounding::Floor);
            v.push((score, *a));
        }
        v.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
        let mut out: Vec<Address> = v.into_iter().map(|x| x.1).collect();
        if ins {
            out.push(INSURANCE);
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn adl_position(
        &mut self,
        user: Address,
        asset: AssetId,
        szi: Sz,
        amount: i128,
        now: TimestampMs,
        ev: &mut Vec<Event>,
        work: &mut VecDeque<Address>,
    ) {
        let total = szi.raw().abs();
        let ranking = self.adl_ranking(asset, &user, szi.0 > 0);
        let mut done = 0i128;
        let mut paid = 0i128;
        let bpx = px_from_ntl(Usd::from_raw(amount), szi, Rounding::Floor);
        for cp in ranking {
            if done == total {
                break;
            }
            let cps = self.position(&cp, asset).raw().abs();
            let take = cps.min(total - done);
            done += take;
            // exact apportionment of `amount` over the chunks
            let cum = if done == total { amount } else { mul_div_wide(amount, done, total, Rounding::Floor) };
            let chunk_amt = cum - paid;
            paid = cum;
            let delta_from = Sz::from_raw(-szi.signum() as i128 * take);
            let r = self.transfer_position(asset, user, cp, delta_from, chunk_amt);
            ev.push(self.fill_event(asset, user, cp, delta_from, chunk_amt, r, LiquidationMethod::Adl, now));
            ev.push(Event::Adl { asset, user, counterparty: cp, sz: Sz::from_raw(take), px: bpx });
            if cp != INSURANCE {
                let cst = &self.accts[self.idx(&cp).expect("cp")];
                let cs = self.cross_sum(cst, None, None, 0);
                let neg = cs.equity < 0
                    || cst.positions.iter().any(|p| !self.lev_in(cst, p.asset).is_cross() && self.iso_equity(p) < 0);
                if neg && !work.contains(&cp) {
                    work.push_back(cp);
                }
            }
        }
        assert_eq!(done, total, "open interest mismatch: not enough ADL counterparties");
    }
}

use interfaces::components::Clearinghouse as _;
