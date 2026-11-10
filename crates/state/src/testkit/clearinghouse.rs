//! Simplified clearinghouse fake (cross margin only, cash-flow accounting,
//! no funding). Conservation is exact: every fill moves `px * sz` cash from
//! buyer to seller and fees to the fee pool, so
//! `Σ balances + Σ spot USDC + insurance + fee pool == flows.net()`.
//! Equity = balance + Σ szi * mark.

use borsh::{BorshDeserialize, BorshSerialize};
use interfaces::*;
use std::collections::{BTreeMap, BTreeSet};

/// Account that absorbs backstopped positions.
pub const BACKSTOP: Address = Address([0xbb; 20]);

#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Acct {
    pub balance: Usd,
    pub spot: BTreeMap<u32, Usd>,
    pub pos: BTreeMap<AssetId, Sz>,
    pub lev: BTreeMap<AssetId, Leverage>,
}

#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct SimpleClearinghouse {
    markets: Vec<MarketSpec>,
    accounts: BTreeMap<Address, Acct>,
    insurance: Usd,
    fee_pool: Usd,
    marks: Vec<Px>,
    oracles: Vec<Px>,
    taker_fee: Rate,
    maker_fee: Rate,
    #[borsh(skip)]
    dirty: BTreeSet<Address>,
    #[borsh(skip)]
    dirty_globals: bool,
}

fn err(s: &str) -> ExecError {
    ExecError::new(s)
}

impl SimpleClearinghouse {
    pub fn new(g: &Genesis) -> Self {
        let mut c = Self {
            markets: g.markets.clone(),
            accounts: BTreeMap::new(),
            insurance: g.params.insurance_fund,
            fee_pool: Usd::ZERO,
            marks: g.markets.iter().map(|m| m.initial_px).collect(),
            oracles: g.markets.iter().map(|m| m.initial_px).collect(),
            taker_fee: g.params.taker_fee,
            maker_fee: g.params.maker_fee,
            dirty: BTreeSet::new(),
            dirty_globals: true,
        };
        assert!(c.maker_fee.0 >= 0, "fake clearinghouse: rebates unsupported");
        for (a, b) in &g.balances {
            c.acct_mut(*a).balance += *b;
        }
        for (a, t, b) in &g.spot_balances {
            *c.acct_mut(*a).spot.entry(*t).or_default() += *b;
        }
        c
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut c = Self::try_from_slice(bytes).map_err(|e| e.to_string())?;
        c.dirty = c.accounts.keys().copied().collect();
        c.dirty_globals = true;
        Ok(c)
    }
    fn acct_mut(&mut self, a: Address) -> &mut Acct {
        self.dirty.insert(a);
        self.accounts.entry(a).or_default()
    }
    fn lev(&self, a: &Address, asset: AssetId) -> u32 {
        self.accounts
            .get(a)
            .and_then(|x| x.lev.get(&asset))
            .map(|l| l.value())
            .unwrap_or_else(|| self.markets[asset as usize].max_leverage)
    }
    pub fn equity(&self, a: &Address) -> Usd {
        let Some(x) = self.accounts.get(a) else { return Usd::ZERO };
        let mut e = x.balance;
        for (asset, szi) in &x.pos {
            e += notional(self.marks[*asset as usize], *szi, Rounding::Floor);
        }
        e
    }
    fn margin_used(&self, a: &Address, override_pos: Option<(AssetId, Sz, Px)>) -> Usd {
        let mut m = Usd::ZERO;
        let mut seen = false;
        if let Some(x) = self.accounts.get(a) {
            for (asset, szi) in &x.pos {
                let (szi, px) = match override_pos {
                    Some((oa, osz, opx)) if oa == *asset => {
                        seen = true;
                        (osz, opx.max(self.marks[oa as usize]))
                    }
                    _ => (*szi, self.marks[*asset as usize]),
                };
                let ntl = notional(px, szi.abs(), Rounding::Ceil);
                m += Usd::from_raw(div_round(ntl.raw(), self.lev(a, *asset) as i128, Rounding::Ceil));
            }
        }
        if let (Some((oa, osz, opx)), false) = (override_pos, seen) {
            let ntl = notional(opx.max(self.marks[oa as usize]), osz.abs(), Rounding::Ceil);
            m += Usd::from_raw(div_round(ntl.raw(), self.lev(a, oa) as i128, Rounding::Ceil));
        }
        m
    }
    fn maintenance(&self, a: &Address) -> Usd {
        let mut m = Usd::ZERO;
        if let Some(x) = self.accounts.get(a) {
            for (asset, szi) in &x.pos {
                let ntl = notional(self.marks[*asset as usize], szi.abs(), Rounding::Ceil);
                m += usd_mul_rate(ntl, self.markets[*asset as usize].maintenance_margin_rate(), Rounding::Ceil);
            }
        }
        m
    }
    fn withdrawable(&self, a: &Address) -> Usd {
        let w = self.equity(a) - self.margin_used(a, None);
        let bal = self.accounts.get(a).map(|x| x.balance).unwrap_or_default();
        w.min(bal).max(Usd::ZERO)
    }
    fn move_pos(&mut self, a: Address, asset: AssetId, delta: Sz) -> Sz {
        let x = self.acct_mut(a);
        let start = x.pos.get(&asset).copied().unwrap_or_default();
        let n = start + delta;
        if n.0 == 0 {
            x.pos.remove(&asset);
        } else {
            x.pos.insert(asset, n);
        }
        start
    }
    pub fn fee_pool(&self) -> Usd {
        self.fee_pool
    }
}

impl Clearinghouse for SimpleClearinghouse {
    fn markets(&self) -> &[MarketSpec] {
        &self.markets
    }
    fn deposit(&mut self, user: Address, amount: Usd) -> Result<(), ExecError> {
        self.acct_mut(user).balance += amount;
        Ok(())
    }
    fn withdraw(&mut self, user: Address, amount: Usd) -> Result<(), ExecError> {
        if self.withdrawable(&user) < amount {
            return Err(err("Insufficient balance for withdrawal."));
        }
        self.acct_mut(user).balance -= amount;
        Ok(())
    }
    fn usd_send(&mut self, from: Address, to: Address, amount: Usd) -> Result<(), ExecError> {
        if self.withdrawable(&from) < amount {
            return Err(err("Insufficient balance for transfer."));
        }
        self.acct_mut(from).balance -= amount;
        self.acct_mut(to).balance += amount;
        Ok(())
    }
    fn spot_send(&mut self, from: Address, to: Address, token: u32, amount: Usd) -> Result<(), ExecError> {
        let have = self.accounts.get(&from).and_then(|x| x.spot.get(&token)).copied().unwrap_or_default();
        if have < amount {
            return Err(err("Insufficient balance for token transfer."));
        }
        let f = self.acct_mut(from);
        let left = have - amount;
        if left.0 == 0 {
            f.spot.remove(&token);
        } else {
            f.spot.insert(token, left);
        }
        *self.acct_mut(to).spot.entry(token).or_default() += amount;
        Ok(())
    }
    fn class_transfer(&mut self, user: Address, amount: Usd, to_perp: bool) -> Result<(), ExecError> {
        if to_perp {
            let have = self.accounts.get(&user).and_then(|x| x.spot.get(&0)).copied().unwrap_or_default();
            if have < amount {
                return Err(err("Insufficient balance."));
            }
            let x = self.acct_mut(user);
            *x.spot.entry(0).or_default() -= amount;
            x.spot.retain(|_, v| v.0 != 0);
            x.balance += amount;
        } else {
            if self.withdrawable(&user) < amount {
                return Err(err("Insufficient balance."));
            }
            let x = self.acct_mut(user);
            x.balance -= amount;
            *x.spot.entry(0).or_default() += amount;
        }
        Ok(())
    }
    fn update_leverage(&mut self, user: Address, asset: AssetId, is_cross: bool, leverage: u32) -> Result<(), ExecError> {
        let l = if is_cross { Leverage::Cross(leverage) } else { Leverage::Isolated(leverage) };
        let old = self.accounts.get(&user).and_then(|x| x.lev.get(&asset)).copied();
        self.acct_mut(user).lev.insert(asset, l);
        if self.equity(&user) < self.margin_used(&user, None) {
            let x = self.acct_mut(user);
            match old {
                Some(o) => x.lev.insert(asset, o),
                None => x.lev.remove(&asset),
            };
            return Err(err("Insufficient margin for leverage update."));
        }
        Ok(())
    }
    fn update_isolated_margin(&mut self, _user: Address, _asset: AssetId, _delta: Usd) -> Result<(), ExecError> {
        Err(err("Isolated margin not supported by the test clearinghouse."))
    }
    fn leverage_of(&self, user: &Address, asset: AssetId) -> Leverage {
        self.accounts
            .get(user)
            .and_then(|x| x.lev.get(&asset))
            .copied()
            .unwrap_or(Leverage::Cross(self.markets[asset as usize].max_leverage))
    }
    fn check_order_margin(&self, c: &MarginCheck) -> Result<(), ExecError> {
        let cur = self.position(&c.user, c.asset);
        let delta = Sz(c.sz.0 * c.side.sign());
        let new = cur + delta;
        if c.reduce_only || (new.abs() <= cur.abs() && new.signum() * cur.signum() >= 0) {
            return Ok(());
        }
        let mark = self.marks[c.asset as usize];
        // Immediate PnL of filling at px relative to mark (negative when paying up).
        let imm = notional(mark, delta, Rounding::Floor) - notional(c.px, delta, Rounding::Ceil);
        let eq_after = self.equity(&c.user) + imm;
        let fee = usd_mul_rate(notional(c.px, c.sz, Rounding::Ceil), self.taker_fee, Rounding::Ceil);
        if eq_after - fee < self.margin_used(&c.user, Some((c.asset, new, c.px))) {
            return Err(err("Insufficient margin to place order."));
        }
        Ok(())
    }
    fn reduce_only_capacity(&self, user: &Address, asset: AssetId, side: Side) -> Sz {
        let p = self.position(user, asset);
        match side {
            Side::Ask if p.0 > 0 => p,
            Side::Bid if p.0 < 0 => -p,
            _ => Sz::ZERO,
        }
    }
    fn apply_fill(&mut self, f: &FillParams) -> Result<FillResult, ExecError> {
        if f.sz.0 <= 0 || f.taker == f.maker {
            return Err(err("invalid fill"));
        }
        let ntl = notional(f.px, f.sz, Rounding::Floor);
        let tfee = usd_mul_rate(ntl, self.taker_fee, Rounding::Ceil);
        let mfee = usd_mul_rate(ntl, self.maker_fee, Rounding::Ceil);
        let (buyer, seller) = match f.taker_side {
            Side::Bid => (f.taker, f.maker),
            Side::Ask => (f.maker, f.taker),
        };
        self.acct_mut(buyer).balance -= ntl;
        self.acct_mut(seller).balance += ntl;
        self.acct_mut(f.taker).balance -= tfee;
        self.acct_mut(f.maker).balance -= mfee;
        self.fee_pool += tfee + mfee;
        self.dirty_globals = true;
        let bs = self.move_pos(buyer, f.asset, f.sz);
        let ss = self.move_pos(seller, f.asset, -f.sz);
        let (ts, ms) = if f.taker_side == Side::Bid { (bs, ss) } else { (ss, bs) };
        Ok(FillResult {
            taker_fee: tfee,
            maker_fee: mfee,
            taker_closed_pnl: Usd::ZERO,
            maker_closed_pnl: Usd::ZERO,
            taker_start_pos: ts,
            maker_start_pos: ms,
        })
    }
    fn position(&self, user: &Address, asset: AssetId) -> Sz {
        self.accounts.get(user).and_then(|x| x.pos.get(&asset)).copied().unwrap_or_default()
    }
    fn set_prices(&mut self, asset: AssetId, oracle_px: Px, mark_px: Px, _mid: Option<Px>, _now: TimestampMs) {
        let a = asset as usize;
        if self.marks[a] != mark_px || self.oracles[a] != oracle_px {
            self.marks[a] = mark_px;
            self.oracles[a] = oracle_px;
            self.dirty_globals = true;
        }
    }
    fn on_block_funding(&mut self, _now: TimestampMs) -> Vec<Event> {
        vec![]
    }
    fn liquidation_targets(&self) -> Vec<LiquidationTarget> {
        let mut v = vec![];
        for (a, x) in &self.accounts {
            if *a == BACKSTOP || x.pos.is_empty() {
                continue;
            }
            if self.equity(a) < self.maintenance(a) {
                let closes = x
                    .pos
                    .iter()
                    .map(|(asset, szi)| (*asset, -*szi, if szi.0 > 0 { Px(1) } else { Px(0) }))
                    .collect();
                v.push(LiquidationTarget { user: *a, closes, isolated_asset: None });
            }
        }
        v
    }
    fn backstop_and_adl(&mut self, user: Address, _now: TimestampMs) -> Vec<Event> {
        let mut ev = vec![];
        if self.equity(&user) < self.maintenance(&user) {
            let pos: Vec<(AssetId, Sz)> =
                self.accounts.get(&user).map(|x| x.pos.iter().map(|(a, s)| (*a, *s)).collect()).unwrap_or_default();
            for (asset, szi) in pos {
                // Backstop takes the position at mark.
                let ntl = notional(self.marks[asset as usize], szi, Rounding::Floor);
                self.acct_mut(user).balance += ntl;
                self.acct_mut(BACKSTOP).balance -= ntl;
                self.move_pos(user, asset, -szi);
                self.move_pos(BACKSTOP, asset, szi);
            }
        }
        let bal = self.accounts.get(&user).map(|x| x.balance).unwrap_or_default();
        let has_pos = self.accounts.get(&user).is_some_and(|x| !x.pos.is_empty());
        if bal.0 < 0 && !has_pos {
            self.insurance -= -bal;
            self.acct_mut(user).balance = Usd::ZERO;
            self.dirty_globals = true;
            ev.push(Event::BadDebtCovered { asset: 0, user, amount: -bal, by_insurance: true });
        }
        ev
    }
    fn account_view(&self, user: &Address) -> AccountView {
        let Some(x) = self.accounts.get(user) else { return AccountView::default() };
        let mut v = AccountView::default();
        let eq = self.equity(user);
        let mu = self.margin_used(user, None);
        let mut tot = Usd::ZERO;
        for (asset, szi) in &x.pos {
            let mark = self.marks[*asset as usize];
            let pv = notional(mark, szi.abs(), Rounding::Floor);
            tot += pv;
            v.positions.push(PositionView {
                asset: *asset,
                szi: *szi,
                entry_px: mark,
                position_value: pv,
                unrealized_pnl: Usd::ZERO,
                return_on_equity: Rate::ZERO,
                leverage: self.leverage_of(user, *asset),
                margin_used: Usd::ZERO,
                liquidation_px: None,
                max_leverage: self.markets[*asset as usize].max_leverage,
                cum_funding_all_time: Usd::ZERO,
                cum_funding_since_open: Usd::ZERO,
            });
        }
        let ms = MarginSummary { account_value: eq, total_ntl_pos: tot, total_raw_usd: x.balance, total_margin_used: mu };
        v.margin_summary = ms.clone();
        v.cross_margin_summary = ms;
        v.cross_maintenance_margin_used = self.maintenance(user);
        v.withdrawable = self.withdrawable(user);
        v.spot_balances = x.spot.iter().filter(|(_, b)| b.0 != 0).map(|(t, b)| (*t, *b)).collect();
        v
    }
    fn market_ctx(&self, asset: AssetId) -> MarketCtx {
        let a = asset as usize;
        MarketCtx {
            asset,
            oracle_px: self.oracles[a],
            mark_px: self.marks[a],
            mid_px: None,
            funding: Rate::ZERO,
            premium: Rate::ZERO,
            open_interest: Sz::ZERO,
            day_ntl_vlm: Usd::ZERO,
            prev_day_px: self.oracles[a],
        }
    }
    fn insurance_fund(&self) -> Usd {
        self.insurance
    }
    fn total_collateral(&self) -> Usd {
        self.accounts.values().fold(Usd::ZERO, |s, x| s + x.balance + x.spot.get(&0).copied().unwrap_or_default())
    }
    fn check_invariants(&self, flows: &ExternalFlows, height: Height) -> Result<(), InvariantViolation> {
        let total = self.total_collateral() + self.insurance + self.fee_pool;
        if total != flows.net() {
            return Err(InvariantViolation {
                height,
                what: format!("conservation: held {total} != net flows {}", flows.net()),
            });
        }
        let mut oi = vec![0i64; self.markets.len()];
        for x in self.accounts.values() {
            for (a, s) in &x.pos {
                oi[*a as usize] += s.0;
            }
        }
        if oi.iter().any(|s| *s != 0) {
            return Err(InvariantViolation { height, what: "positions do not net to zero".into() });
        }
        Ok(())
    }
    fn take_dirty(&mut self) -> Vec<(StateKey, Hash32)> {
        let mut v = vec![];
        for a in std::mem::take(&mut self.dirty) {
            let h = match self.accounts.get(&a) {
                Some(x) => hash_borsh(x),
                None => Hash32::ZERO,
            };
            v.push((StateKey::Account(a), h));
        }
        if self.dirty_globals {
            self.dirty_globals = false;
            v.push((StateKey::Insurance, hash_borsh(&(self.insurance, self.fee_pool))));
            for i in 0..self.markets.len() {
                v.push((StateKey::Market(i as AssetId), hash_borsh(&(self.marks[i], self.oracles[i]))));
            }
        }
        v
    }
    fn encode(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("encode")
    }
}
