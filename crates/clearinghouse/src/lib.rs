//! Cross/isolated-margin perpetual futures clearinghouse (HL-compatible
//! semantics), deterministic fixed-point. See `README.md` for the formulas,
//! rounding rules and the cash-accounting design that makes conservation
//! exact.

pub mod math;
pub mod state;

mod liq;
mod views;

use borsh::BorshDeserialize;
use interfaces::components::Clearinghouse as ClearinghouseApi;
use interfaces::*;
use math::*;
pub use state::*;
use std::collections::{BTreeSet, HashMap, VecDeque};

// ------------------------------------------------------------ errors ---

pub const ERR_MARGIN: &str = "Insufficient margin to place order.";
pub const ERR_WITHDRAW: &str = "Insufficient balance for withdrawal";
pub const ERR_TRANSFER: &str = "Insufficient balance for transfer";
pub const ERR_SPOT: &str = "Insufficient balance for token transfer";
pub const ERR_AMOUNT: &str = "Invalid amount";
pub const ERR_ASSET: &str = "Invalid asset";
pub const ERR_TOKEN: &str = "Invalid token";
pub const ERR_LEVERAGE: &str = "Invalid leverage value";
pub const ERR_CROSS_NOT_ALLOWED: &str = "Cross margin is not allowed for this asset.";
pub const ERR_SWITCH_MODE: &str = "Cannot switch leverage type with open position.";
pub const ERR_LEVERAGE_MARGIN: &str = "Insufficient margin to decrease leverage.";
pub const ERR_NO_ISO_POSITION: &str = "No isolated position to update margin.";
pub const ERR_ISO_ADD: &str = "Insufficient margin to add.";
pub const ERR_ISO_REMOVE: &str = "Insufficient margin to remove.";
pub const ERR_USER: &str = "Invalid user";
pub const ERR_FILL: &str = "Invalid fill";

/// ceil(n / d) for n >= 0 using 64-bit division when possible.
#[inline]
pub(crate) fn div_ceil_small(n: i128, d: u64) -> i128 {
    debug_assert!(n >= 0 && d > 0);
    if n <= u64::MAX as i128 {
        (n as u64).div_ceil(d) as i128
    } else {
        div_round(n, d as i128, Rounding::Ceil)
    }
}

fn err<T>(s: &str) -> Result<T, ExecError> {
    Err(ExecError::new(s))
}

/// Premium sampling period (block time).
pub const PREMIUM_SAMPLE_MS: u64 = 5_000;
/// Clamp applied to (interest − premium) in the funding formula: 0.05%.
pub const INTEREST_CLAMP: Rate = Rate(500_000_000);
/// HL default leverage for a fresh (account, asset).
pub const DEFAULT_LEVERAGE: u32 = 20;
const DAY_MS: u64 = 86_400_000;
/// Full invariant recomputation runs when there are at most this many
/// accounts, or every `FULL_CHECK_EVERY` heights.
pub const FULL_CHECK_MAX_ACCOUNTS: usize = 4096;
pub const FULL_CHECK_EVERY: u64 = 64;

// ------------------------------------------------------------ runtime ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LiqState {
    /// No positions, no debt: never liquidatable until mutated.
    Clean,
    /// Mutated since last risk evaluation: must be checked exactly.
    Stale,
    /// Last evaluation: liquidatable (or has debt).
    AtRisk,
    /// Safe while every held market's mark stays inside its band.
    Banded,
}

#[derive(Clone, Debug)]
pub(crate) struct Meta {
    pub dirty: bool,
    pub liq: LiqState,
    /// Band entries currently inserted in the per-market indices.
    pub bands: Vec<(AssetId, Px, Px)>,
}

/// Aggregate of an account's cross positions at current marks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrossSum {
    /// cash + Σ szi·mark (Floor per position).
    pub equity: i128,
    /// Σ ceil(|ntl| / leverage).
    pub im: i128,
    /// Σ ceil(|ntl| · mmr).
    pub mm: i128,
    /// Σ |ntl| (Ceil).
    pub ntl: i128,
    /// Σ |ntl|·(1 + mmr) (Ceil) — sensitivity bound used by the liquidation index.
    pub sens: i128,
    /// Number of cross positions.
    pub n: usize,
}

/// Result of applying a size change to one side's position (pure plan).
#[derive(Clone, Debug)]
pub(crate) struct TradeOut {
    pub pos: Position,
    pub cross_delta: i128,
    pub deficit_delta: i128,
    pub closed_pnl: i128,
    pub risk_up: bool,
}

/// The clearinghouse. Construct with [`Clearinghouse::new`] from genesis or
/// [`Clearinghouse::decode`] from a snapshot.
#[derive(Clone)]
pub struct Clearinghouse {
    pub(crate) specs: Vec<MarketSpec>,
    pub(crate) params: ChainParams,
    pub(crate) spot_tokens: Vec<SpotToken>,
    pub(crate) mmr: Vec<Rate>,
    pub(crate) markets: Vec<MarketState>,
    /// idx 0 is always the insurance fund.
    pub(crate) accts: Vec<AccountState>,
    pub(crate) meta: Vec<Meta>,
    /// Lookup only (never iterated).
    pub(crate) index: HashMap<Address, u32>,
    pub(crate) ledger: Ledger,
    /// Holders (nonzero szi) per market, ascending address.
    pub(crate) holders: Vec<BTreeSet<Address>>,
    /// Running total of every USDC bucket (cross cash, isolated cash,
    /// deficits, spot USDC, fee pool). Updated by every mutation.
    pub(crate) total: i128,
    pub(crate) dirty_list: Vec<u32>,
    pub(crate) dirty_markets: Vec<bool>,
    pub(crate) dirty_insurance: bool,
    pub(crate) stale: Vec<u32>,
    pub(crate) at_risk: BTreeSet<u32>,
    pub(crate) band_lo: Vec<BTreeSet<(Px, u32)>>,
    pub(crate) band_hi: Vec<BTreeSet<(Px, u32)>>,
}

const SNAPSHOT_VERSION: u32 = 1;

impl Clearinghouse {
    pub fn new(g: &Genesis) -> Self {
        let n = g.markets.len();
        for (i, m) in g.markets.iter().enumerate() {
            assert_eq!(m.asset as usize, i, "market asset ids must equal their index");
            assert!(m.max_leverage >= 1, "max_leverage >= 1");
            assert!(m.initial_px.0 > 0, "initial_px > 0");
        }
        let t = g.genesis_time_ms;
        let markets = g
            .markets
            .iter()
            .map(|m| MarketState {
                oracle_px: m.initial_px,
                mark_px: m.initial_px,
                mid_px: None,
                premium_sum: 0,
                premium_samples: 0,
                last_premium: Rate::ZERO,
                last_sample_slot: t / PREMIUM_SAMPLE_MS,
                last_funding_slot: t / g.params.funding_interval_ms.max(1),
                last_funding_rate: Rate::ZERO,
                last_funding_time: t,
                oi_long: Sz::ZERO,
                oi_short: Sz::ZERO,
                day: t / DAY_MS,
                day_ntl_vlm: Usd::ZERO,
                prev_day_px: m.initial_px,
                funding_paid: Usd::ZERO,
                funding_received: Usd::ZERO,
                funding_dust: Usd::ZERO,
            })
            .collect();
        let mut ins = AccountState::new(INSURANCE);
        ins.cash = g.params.insurance_fund;
        let mut accounts: std::collections::BTreeMap<Address, AccountState> = Default::default();
        for (a, amt) in &g.balances {
            assert!(*a != INSURANCE && amt.0 >= 0);
            let st = accounts.entry(*a).or_insert_with(|| AccountState::new(*a));
            st.cash += *amt;
        }
        let mut supply = vec![Usd::ZERO; g.spot_tokens.len()];
        for (a, tok, amt) in &g.spot_balances {
            assert!(*a != INSURANCE && amt.0 >= 0);
            assert!((*tok as usize) < g.spot_tokens.len(), "unknown spot token in genesis");
            let st = accounts.entry(*a).or_insert_with(|| AccountState::new(*a));
            st.spot_add(*tok, *amt);
            supply[*tok as usize] += *amt;
        }
        let mut list = vec![ins];
        list.extend(accounts.into_values());
        let snap = ChSnapshot {
            version: SNAPSHOT_VERSION,
            specs: g.markets.clone(),
            params: g.params.clone(),
            spot_tokens: g.spot_tokens.clone(),
            markets,
            accounts: list,
            ledger: Ledger { spot_supply: supply, ..Default::default() },
        };
        let mut ch = Self::from_snapshot(snap);
        // everything is "dirty" at genesis
        // idx 0 (insurance) is reported only under StateKey::Insurance
        ch.dirty_list = (1..ch.accts.len() as u32).collect();
        for m in ch.meta.iter_mut() {
            m.dirty = true;
        }
        ch.dirty_insurance = true;
        ch.dirty_markets = vec![true; n];
        ch
    }

    /// Value that genesis puts into the clearinghouse (`ExternalFlows.genesis`):
    /// perp balances + spot USDC + insurance fund.
    pub fn genesis_total(g: &Genesis) -> Usd {
        let mut t = g.params.insurance_fund;
        for (_, a) in &g.balances {
            t += *a;
        }
        for (_, tok, a) in &g.spot_balances {
            if *tok == 0 {
                t += *a;
            }
        }
        t
    }

    /// Restore from [`ClearinghouseApi::encode`] output.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let snap = ChSnapshot::try_from_slice(bytes).map_err(|e| e.to_string())?;
        if snap.version != SNAPSHOT_VERSION {
            return Err(format!("unsupported clearinghouse snapshot version {}", snap.version));
        }
        if snap.accounts.first().map(|a| a.address) != Some(INSURANCE) && !snap.accounts.iter().any(|a| a.address == INSURANCE) {
            return Err("snapshot lacks insurance account".into());
        }
        if snap.markets.len() != snap.specs.len() {
            return Err("market count mismatch".into());
        }
        Ok(Self::from_snapshot(snap))
    }

    fn from_snapshot(snap: ChSnapshot) -> Self {
        let n = snap.specs.len();
        let mmr = snap.specs.iter().map(|s| s.maintenance_margin_rate()).collect();
        // insurance first, then the rest in their (sorted) order
        let mut accts = Vec::with_capacity(snap.accounts.len());
        let mut rest = Vec::with_capacity(snap.accounts.len());
        for a in snap.accounts {
            if a.address == INSURANCE {
                accts.push(a);
            } else {
                rest.push(a);
            }
        }
        accts.extend(rest);
        let mut ch = Self {
            specs: snap.specs,
            params: snap.params,
            spot_tokens: snap.spot_tokens,
            mmr,
            markets: snap.markets,
            meta: Vec::new(),
            index: HashMap::new(),
            ledger: snap.ledger,
            holders: vec![BTreeSet::new(); n],
            total: 0,
            dirty_list: Vec::new(),
            dirty_markets: vec![false; n],
            dirty_insurance: false,
            stale: Vec::new(),
            at_risk: BTreeSet::new(),
            band_lo: vec![BTreeSet::new(); n],
            band_hi: vec![BTreeSet::new(); n],
            accts,
        };
        let mut total = ch.ledger.fee_pool.raw();
        for (i, a) in ch.accts.iter().enumerate() {
            ch.index.insert(a.address, i as u32);
            total += a.cash.raw() + a.iso_deficit.raw() + a.spot_bal(0).raw();
            for p in &a.positions {
                total += p.iso_cash.raw();
                ch.holders[p.asset as usize].insert(a.address);
            }
        }
        ch.total = total;
        ch.meta = vec![Meta { dirty: false, liq: LiqState::Clean, bands: Vec::new() }; ch.accts.len()];
        for i in 1..ch.accts.len() as u32 {
            ch.meta[i as usize].liq = LiqState::Stale;
            ch.stale.push(i);
        }
        ch
    }

    // ------------------------------------------------------- helpers ---

    pub fn params(&self) -> &ChainParams {
        &self.params
    }
    pub fn spot_tokens(&self) -> &[SpotToken] {
        &self.spot_tokens
    }
    pub fn account(&self, user: &Address) -> Option<&AccountState> {
        self.index.get(user).map(|i| &self.accts[*i as usize])
    }
    pub fn market_state(&self, asset: AssetId) -> &MarketState {
        &self.markets[asset as usize]
    }
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }
    /// All account addresses (ascending), insurance excluded.
    pub fn addresses(&self) -> Vec<Address> {
        let mut v: Vec<Address> = self.accts[1..].iter().map(|a| a.address).collect();
        v.sort();
        v
    }
    /// Spot balance of `token`.
    pub fn spot_balance(&self, user: &Address, token: u32) -> Usd {
        self.account(user).map(|a| a.spot_bal(token)).unwrap_or(Usd::ZERO)
    }

    pub(crate) fn idx(&self, a: &Address) -> Option<usize> {
        self.index.get(a).map(|i| *i as usize)
    }

    pub(crate) fn get_or_create(&mut self, a: Address) -> usize {
        if let Some(i) = self.index.get(&a) {
            return *i as usize;
        }
        let i = self.accts.len();
        self.accts.push(AccountState::new(a));
        self.meta.push(Meta { dirty: false, liq: LiqState::Clean, bands: Vec::new() });
        self.index.insert(a, i as u32);
        i
    }

    pub(crate) fn touch(&mut self, i: usize) {
        if i == 0 {
            self.dirty_insurance = true;
            return;
        }
        let m = &mut self.meta[i];
        if !m.dirty {
            m.dirty = true;
            self.dirty_list.push(i as u32);
        }
        if m.liq != LiqState::Stale {
            m.liq = LiqState::Stale;
            self.stale.push(i as u32);
        }
    }

    pub(crate) fn add_cash(&mut self, i: usize, d: i128) {
        let st = &mut self.accts[i];
        st.cash = Usd::from_raw(st.cash.raw() + d);
        self.total += d;
        self.touch(i);
    }

    pub(crate) fn spec(&self, asset: AssetId) -> Result<&MarketSpec, ExecError> {
        self.specs.get(asset as usize).ok_or_else(|| ExecError::new(ERR_ASSET))
    }

    pub(crate) fn default_lev(&self, asset: AssetId) -> Leverage {
        let s = &self.specs[asset as usize];
        let v = DEFAULT_LEVERAGE.min(s.max_leverage);
        if s.only_isolated {
            Leverage::Isolated(v)
        } else {
            Leverage::Cross(v)
        }
    }

    pub(crate) fn lev_in(&self, st: &AccountState, asset: AssetId) -> Leverage {
        if st.address == INSURANCE {
            return Leverage::Cross(self.specs[asset as usize].max_leverage);
        }
        st.setting(asset).map(|s| s.leverage).unwrap_or_else(|| self.default_lev(asset))
    }

    #[inline]
    pub(crate) fn mark(&self, asset: AssetId) -> Px {
        self.markets[asset as usize].mark_px
    }
    /// Signed position value at mark, Floor.
    #[inline]
    pub(crate) fn value(&self, asset: AssetId, szi: Sz) -> i128 {
        self.val_ntl(asset, szi).0
    }
    /// |notional| at mark, Ceil.
    #[inline]
    pub(crate) fn ntl_abs(&self, asset: AssetId, szi: Sz) -> i128 {
        self.val_ntl(asset, szi).1
    }
    /// (signed value Floor, |notional| Ceil) at mark with a single division;
    /// identical to `notional(mark, szi, Floor)` / `notional(mark, |szi|, Ceil)`.
    #[inline]
    pub(crate) fn val_ntl(&self, asset: AssetId, szi: Sz) -> (i128, i128) {
        const D: u128 = (PX_SCALE * SZ_SCALE / USD_SCALE) as u128;
        let p = self.mark(asset).0 as u128 * szi.0.unsigned_abs() as u128;
        let q = p / D;
        let up = (q + (q * D != p) as u128) as i128;
        if szi.0 >= 0 {
            (q as i128, up)
        } else {
            (-up, up)
        }
    }
    /// Maintenance margin = ceil(|ntl| / (2·max_leverage)) (exact HL fraction).
    #[inline]
    pub(crate) fn mm_of(&self, asset: AssetId, ntl: i128) -> i128 {
        div_ceil_small(ntl, 2 * self.specs[asset as usize].max_leverage as u64)
    }
    /// Sensitivity bound |ntl|·(1 + mmr) (rounded up).
    #[inline]
    pub(crate) fn sens_of(&self, asset: AssetId, ntl: i128) -> i128 {
        ntl + self.mm_of(asset, ntl)
    }

    /// Cross aggregate, optionally with one position replaced (`over_pos`,
    /// szi may be 0 = removed), one leverage overridden, and cash shifted.
    pub(crate) fn cross_sum(
        &self,
        st: &AccountState,
        over_pos: Option<&Position>,
        over_lev: Option<(AssetId, Leverage)>,
        cash_delta: i128,
    ) -> CrossSum {
        let mut s = CrossSum { equity: st.cash.raw() + cash_delta, ..Default::default() };
        let add = |p: &Position, s: &mut CrossSum| {
            let lev = match over_lev {
                Some((a, l)) if a == p.asset => l,
                _ => p.leverage,
            };
            if !lev.is_cross() {
                return;
            }
            let (v, ntl) = self.val_ntl(p.asset, p.szi);
            let mm = self.mm_of(p.asset, ntl);
            s.equity += v;
            s.im += div_ceil_small(ntl, lev.value() as u64);
            s.mm += mm;
            s.ntl += ntl;
            s.sens += ntl + mm;
            s.n += 1;
        };
        let over_asset = over_pos.map(|p| p.asset);
        for p in &st.positions {
            if Some(p.asset) == over_asset {
                continue;
            }
            add(p, &mut s);
        }
        if let Some(p) = over_pos {
            if !p.szi.is_zero() {
                add(p, &mut s);
            }
        }
        s
    }

    /// Isolated position equity = iso_cash + szi·mark.
    pub(crate) fn iso_equity(&self, p: &Position) -> i128 {
        p.iso_cash.raw() + self.value(p.asset, p.szi)
    }

    /// Cross "available" = equity − initial margin (may be negative).
    pub(crate) fn cross_available(&self, st: &AccountState) -> i128 {
        let s = self.cross_sum(st, None, None, 0);
        s.equity - s.im
    }

    /// HL withdrawable: max(0, cross account value − cross initial margin).
    pub fn withdrawable(&self, user: &Address) -> Usd {
        match self.account(user) {
            Some(st) => Usd::from_raw(self.cross_available(st).max(0)),
            None => Usd::ZERO,
        }
    }

    pub fn fee_for(&self, n: i128, rate: Rate) -> i128 {
        // payers (rate >= 0) round up; rebates (rate < 0) round down in magnitude
        let n = n.abs();
        if rate.0 >= 0 {
            mul_div(n, rate.raw(), RATE_SCALE, Rounding::Ceil)
        } else {
            -mul_div(n, -rate.raw(), RATE_SCALE, Rounding::Floor)
        }
    }

    /// Pure position update: apply signed size change `d` with the buyer
    /// paying `n` (signed) to the seller, fee `fee` charged to cross cash.
    /// See README "Fills".
    pub(crate) fn trade(&self, pos0: &Position, lev: Leverage, d: Sz, n: i128, fee: i128, liq: bool) -> TradeOut {
        let asset = pos0.asset;
        let s0 = pos0.szi.raw();
        let b0 = pos0.basis.raw();
        let dd = d.raw();
        assert!(dd != 0);
        let ds = dd.signum();
        let ad = dd.abs();
        let as0 = s0.abs();
        let q = if s0 != 0 && s0.signum() != ds { ad.min(as0) } else { 0 };
        let ncl = if q == 0 {
            0
        } else if q == ad {
            n
        } else {
            mul_div_wide(n, q, ad, Rounding::Floor)
        };
        let nop = n - ncl;
        let r = if q == 0 {
            0
        } else if q == as0 {
            b0
        } else {
            mul_div_wide(b0, q, as0, Rounding::TowardZero)
        };
        let closed = if q > 0 { s0.signum() * ncl - r } else { 0 };
        let s_mid = s0 + ds * q;
        let open = ad - q;
        let s1 = s_mid + ds * open;
        let b1 = b0 - r + ds * nop;
        let mut cross = -fee;
        let mut deficit = 0i128;
        let mut iso = pos0.iso_cash.raw();
        if lev.is_cross() {
            cross -= ds * n;
            iso = 0;
        } else {
            iso -= ds * ncl;
            if q > 0 {
                if s_mid == 0 {
                    if iso >= 0 || !liq {
                        cross += iso;
                    } else {
                        deficit += iso;
                    }
                    iso = 0;
                } else {
                    let e = iso + self.value(asset, Sz::from_raw(s_mid));
                    if e > 0 {
                        let rel = mul_div_wide(e, q, as0, Rounding::Floor);
                        iso -= rel;
                        cross += rel;
                    }
                }
            }
            if open > 0 {
                iso -= ds * nop;
                let t = div_round(nop.abs(), lev.value() as i128, Rounding::Ceil);
                iso += t;
                cross -= t;
            }
        }
        let fresh = s0 == 0 || (s1 != 0 && s1.signum() != s0.signum());
        let pos = Position {
            asset,
            leverage: lev,
            szi: Sz::from_raw(s1),
            basis: Usd::from_raw(if s1 == 0 { 0 } else { b1 }),
            iso_cash: Usd::from_raw(iso),
            cum_funding_since_open: if fresh { Usd::ZERO } else { pos0.cum_funding_since_open },
            cum_funding_since_change: Usd::ZERO,
        };
        let risk_up = s1.abs() > as0 || (s0 != 0 && s1 != 0 && s1.signum() != s0.signum());
        TradeOut { pos, cross_delta: cross, deficit_delta: deficit, closed_pnl: closed, risk_up }
    }

    /// Plan one side of a trade for `user` (not committed).
    pub(crate) fn plan(&self, user: &Address, asset: AssetId, d: Sz, n: i128, fee: i128, liq: bool) -> (Sz, TradeOut) {
        self.plan_at(self.idx(user), user, asset, d, n, fee, liq)
    }

    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub(crate) fn plan_at(&self, idx: Option<usize>, user: &Address, asset: AssetId, d: Sz, n: i128, fee: i128, liq: bool) -> (Sz, TradeOut) {
        let empty = AccountState::new(*user);
        let st = idx.map(|i| &self.accts[i]).unwrap_or(&empty);
        let lev = self.lev_in(st, asset);
        let pos0 = st.pos(asset).cloned().unwrap_or_else(|| Position::empty(asset, lev));
        (pos0.szi, self.trade(&pos0, lev, d, n, fee, liq))
    }

    /// Initial-margin check after a planned trade (only for risk-increasing
    /// trades): cross equity − cross initial margin >= 0 after the trade.
    pub(crate) fn plan_ok(&self, idx: Option<usize>, out: &TradeOut) -> bool {
        if !out.risk_up {
            return true;
        }
        let empty = AccountState::default();
        let st = idx.map(|i| &self.accts[i]).unwrap_or(&empty);
        let (equity, im) = self.cross_equity_im(st, &out.pos, out.cross_delta);
        equity - im >= 0
    }

    /// Lean version of `cross_sum` (equity and initial margin only) with one
    /// position replaced; used on the fill hot path.
    #[inline]
    pub(crate) fn cross_equity_im(&self, st: &AccountState, over: &Position, cash_delta: i128) -> (i128, i128) {
        let mut equity = st.cash.raw() + cash_delta;
        let mut im = 0i128;
        let mut add = |p: &Position| {
            let lev = p.leverage;
            if lev.is_cross() {
                let (v, ntl) = self.val_ntl(p.asset, p.szi);
                equity += v;
                im += div_ceil_small(ntl, lev.value() as u64);
            }
        };
        for p in &st.positions {
            if p.asset != over.asset {
                add(p);
            }
        }
        if !over.szi.is_zero() {
            add(over);
        }
        (equity, im)
    }

    /// Commit a planned side.
    pub(crate) fn commit(&mut self, user: Address, asset: AssetId, out: TradeOut) {
        let i = self.get_or_create(user);
        self.commit_at(i, user, asset, out)
    }

    pub(crate) fn commit_at(&mut self, i: usize, user: Address, asset: AssetId, out: TradeOut) {
        let default_lev = self.default_lev(asset);
        let st = &mut self.accts[i];
        let old = st.pos(asset).cloned();
        let s0 = old.as_ref().map(|p| p.szi).unwrap_or(Sz::ZERO);
        let iso0 = old.as_ref().map(|p| p.iso_cash.raw()).unwrap_or(0);
        let s1 = out.pos.szi;
        st.cash = Usd::from_raw(st.cash.raw() + out.cross_delta);
        st.iso_deficit = Usd::from_raw(st.iso_deficit.raw() + out.deficit_delta);
        self.total += out.cross_delta + out.deficit_delta + out.pos.iso_cash.raw() - iso0;
        if s0.is_zero() && !s1.is_zero() && user != INSURANCE {
            st.setting_mut(asset, default_lev);
        }
        st.put_pos(out.pos);
        if s0.is_zero() != s1.is_zero() {
            if s1.is_zero() {
                self.holders[asset as usize].remove(&user);
            } else {
                self.holders[asset as usize].insert(user);
            }
        }
        let m = &mut self.markets[asset as usize];
        m.oi_long = m.oi_long + s1.max(Sz::ZERO) - s0.max(Sz::ZERO);
        m.oi_short = m.oi_short + (-s1).max(Sz::ZERO) - (-s0).max(Sz::ZERO);
        self.dirty_markets[asset as usize] = true;
        self.touch(i);
    }

    /// Move `delta_from` of `from`'s position to `to` (who trades the
    /// opposite size), with `from` receiving `amount` cash (signed). No fees,
    /// no margin checks (used by backstop and ADL). Returns
    /// (closed_from, closed_to, start_from, start_to).
    pub(crate) fn transfer_position(
        &mut self,
        asset: AssetId,
        from: Address,
        to: Address,
        delta_from: Sz,
        amount: i128,
    ) -> (Usd, Usd, Sz, Sz) {
        let n = -delta_from.signum() as i128 * amount;
        let (sf, of) = self.plan(&from, asset, delta_from, n, 0, true);
        let (st_, ot) = self.plan(&to, asset, -delta_from, n, 0, false);
        let cf = Usd::from_raw(of.closed_pnl);
        let ct = Usd::from_raw(ot.closed_pnl);
        self.commit(from, asset, of);
        self.commit(to, asset, ot);
        (cf, ct, sf, st_)
    }

    fn check_user(&self, user: &Address) -> Result<(), ExecError> {
        if *user == INSURANCE {
            return err(ERR_USER);
        }
        Ok(())
    }

    /// Withdrawal with fee (HL withdraw3): debits `amount` from perp
    /// withdrawable, credits `fee` to the fee pool. The value leaving the
    /// system is `amount − fee` (the state crate must add exactly that to
    /// `ExternalFlows.withdrawals`).
    pub fn withdraw_with_fee(&mut self, user: Address, amount: Usd, fee: Usd) -> Result<(), ExecError> {
        self.check_user(&user)?;
        if amount.0 <= 0 || fee.0 < 0 || fee > amount {
            return err(ERR_AMOUNT);
        }
        if self.withdrawable(&user) < amount {
            return err(ERR_WITHDRAW);
        }
        let i = self.idx(&user).expect("withdrawable > 0 implies account");
        self.add_cash(i, -amount.raw());
        self.ledger.fee_pool += fee;
        self.total += fee.raw();
        self.dirty_insurance = true;
        Ok(())
    }

    /// Accounts that currently have negative equity (cross or any isolated
    /// position) or an uncovered isolated deficit. Must be empty after a
    /// liquidation pass (AC4 invariant 3). O(accounts).
    pub fn negative_equity_accounts(&self) -> Vec<Address> {
        let mut out = Vec::new();
        for st in &self.accts[1..] {
            let cs = self.cross_sum(st, None, None, 0);
            let mut bad = cs.equity < 0 || st.iso_deficit.0 < 0;
            for p in &st.positions {
                if !self.lev_in(st, p.asset).is_cross() && self.iso_equity(p) < 0 {
                    bad = true;
                }
            }
            if bad {
                out.push(st.address);
            }
        }
        out.sort();
        out
    }

    // ------------------------------------------------------- funding ---

    pub(crate) fn funding_rate_from(&self, m: &MarketState) -> Rate {
        let avg = if m.premium_samples > 0 {
            div_round(m.premium_sum, m.premium_samples as i128, Rounding::TowardZero)
        } else {
            0
        };
        let interest = self.params.funding_interest.raw();
        let c = INTEREST_CLAMP.raw();
        let adj = (interest - avg).clamp(-c, c);
        let cap = self.params.funding_cap.raw().abs();
        Rate::from_raw((avg + adj).clamp(-cap, cap))
    }

    fn settle_funding(&mut self, asset: AssetId, now: TimestampMs, ev: &mut Vec<Event>) {
        let rate = self.funding_rate_from(&self.markets[asset as usize]);
        let oracle = self.markets[asset as usize].oracle_px;
        let holders: Vec<Address> = self.holders[asset as usize].iter().copied().collect();
        let mut paid = 0i128;
        let mut received = 0i128;
        for a in holders {
            let i = self.idx(&a).expect("holder exists");
            let is_cross = self.lev_in(&self.accts[i], asset).is_cross();
            let st = &mut self.accts[i];
            let k = st.positions.binary_search_by_key(&asset, |p| p.asset).expect("holder has position");
            let szi = st.positions[k].szi;
            // owed > 0: user pays (rounded up); owed < 0: user receives |owed| rounded down.
            let owed = px_sz_rate(oracle, szi, rate, Rounding::Ceil);
            if owed.0 > 0 {
                paid += owed.raw();
            } else {
                received -= owed.raw();
            }
            {
                let p = &mut st.positions[k];
                if !is_cross {
                    p.iso_cash -= owed;
                }
                p.cum_funding_since_open += owed;
                p.cum_funding_since_change += owed;
            }
            if is_cross {
                st.cash -= owed;
            }
            if a != INSURANCE {
                let dl = self.default_lev(asset);
                self.accts[i].setting_mut(asset, dl).cum_funding_all_time += owed;
            }
            self.total -= owed.raw();
            self.touch(i);
            ev.push(Event::Funding { user: a, asset, usdc: -owed, szi, funding_rate: rate, time: now });
        }
        let dust = paid - received;
        assert!(dust >= 0, "funding dust negative");
        self.add_cash(0, dust);
        self.ledger.funding_dust_total += Usd::from_raw(dust);
        let m = &mut self.markets[asset as usize];
        m.funding_paid += Usd::from_raw(paid);
        m.funding_received += Usd::from_raw(received);
        m.funding_dust += Usd::from_raw(dust);
        m.premium_sum = 0;
        m.premium_samples = 0;
        m.last_funding_rate = rate;
        m.last_funding_time = now;
        self.dirty_markets[asset as usize] = true;
        self.dirty_insurance = true;
    }
}

// ================================================================ trait ===

impl ClearinghouseApi for Clearinghouse {
    fn markets(&self) -> &[MarketSpec] {
        &self.specs
    }

    fn deposit(&mut self, user: Address, amount: Usd) -> Result<(), ExecError> {
        self.check_user(&user)?;
        if amount.0 <= 0 {
            return err(ERR_AMOUNT);
        }
        let i = self.get_or_create(user);
        self.add_cash(i, amount.raw());
        Ok(())
    }

    fn withdraw(&mut self, user: Address, amount: Usd) -> Result<(), ExecError> {
        self.withdraw_with_fee(user, amount, Usd::ZERO)
    }

    fn usd_send(&mut self, from: Address, to: Address, amount: Usd) -> Result<(), ExecError> {
        self.check_user(&from)?;
        self.check_user(&to)?;
        if amount.0 <= 0 {
            return err(ERR_AMOUNT);
        }
        if self.withdrawable(&from) < amount {
            return err(ERR_TRANSFER);
        }
        if from == to {
            return Ok(());
        }
        let f = self.idx(&from).expect("account");
        self.add_cash(f, -amount.raw());
        let t = self.get_or_create(to);
        self.add_cash(t, amount.raw());
        Ok(())
    }

    fn spot_send(&mut self, from: Address, to: Address, token: u32, amount: Usd) -> Result<(), ExecError> {
        self.check_user(&from)?;
        self.check_user(&to)?;
        if token as usize >= self.spot_tokens.len() {
            return err(ERR_TOKEN);
        }
        if amount.0 <= 0 {
            return err(ERR_AMOUNT);
        }
        if self.spot_balance(&from, token) < amount {
            return err(ERR_SPOT);
        }
        if from == to {
            return Ok(());
        }
        let f = self.idx(&from).expect("account");
        self.accts[f].spot_add(token, -amount);
        self.touch(f);
        let t = self.get_or_create(to);
        self.accts[t].spot_add(token, amount);
        self.touch(t);
        Ok(())
    }

    fn class_transfer(&mut self, user: Address, amount: Usd, to_perp: bool) -> Result<(), ExecError> {
        self.check_user(&user)?;
        if amount.0 <= 0 || self.spot_tokens.is_empty() {
            return err(ERR_AMOUNT);
        }
        if to_perp {
            if self.spot_balance(&user, 0) < amount {
                return err(ERR_TRANSFER);
            }
            let i = self.idx(&user).expect("account");
            self.accts[i].spot_add(0, -amount);
            self.total -= amount.raw();
            self.add_cash(i, amount.raw());
        } else {
            if self.withdrawable(&user) < amount {
                return err(ERR_TRANSFER);
            }
            let i = self.idx(&user).expect("account");
            self.add_cash(i, -amount.raw());
            self.accts[i].spot_add(0, amount);
            self.total += amount.raw();
        }
        Ok(())
    }

    fn update_leverage(&mut self, user: Address, asset: AssetId, is_cross: bool, leverage: u32) -> Result<(), ExecError> {
        self.check_user(&user)?;
        let spec = self.spec(asset)?;
        if leverage == 0 || leverage > spec.max_leverage {
            return err(ERR_LEVERAGE);
        }
        if is_cross && spec.only_isolated {
            return err(ERR_CROSS_NOT_ALLOWED);
        }
        let new = if is_cross { Leverage::Cross(leverage) } else { Leverage::Isolated(leverage) };
        if let Some(st) = self.account(&user) {
            let cur = self.lev_in(st, asset);
            if let Some(p) = st.pos(asset) {
                if cur.is_cross() != is_cross {
                    return err(ERR_SWITCH_MODE);
                }
                if leverage < cur.value() {
                    if is_cross {
                        let s = self.cross_sum(st, None, Some((asset, new)), 0);
                        if s.equity - s.im < 0 {
                            return err(ERR_LEVERAGE_MARGIN);
                        }
                    } else {
                        let need = div_round(self.ntl_abs(asset, p.szi), leverage as i128, Rounding::Ceil);
                        if self.iso_equity(p) < need {
                            return err(ERR_LEVERAGE_MARGIN);
                        }
                    }
                }
            }
        }
        let i = self.get_or_create(user);
        let dl = self.default_lev(asset);
        let st = &mut self.accts[i];
        st.setting_mut(asset, dl).leverage = new;
        if let Ok(k) = st.positions.binary_search_by_key(&asset, |p| p.asset) {
            st.positions[k].leverage = new;
        }
        self.touch(i);
        Ok(())
    }

    fn update_isolated_margin(&mut self, user: Address, asset: AssetId, delta: Usd) -> Result<(), ExecError> {
        self.check_user(&user)?;
        self.spec(asset)?;
        if delta.is_zero() {
            return err(ERR_AMOUNT);
        }
        let Some(i) = self.idx(&user) else { return err(ERR_NO_ISO_POSITION) };
        let st = &self.accts[i];
        let lev = self.lev_in(st, asset);
        let Some(p) = st.pos(asset) else { return err(ERR_NO_ISO_POSITION) };
        if lev.is_cross() {
            return err(ERR_NO_ISO_POSITION);
        }
        if delta.0 > 0 {
            if self.cross_available(st) < delta.raw() {
                return err(ERR_ISO_ADD);
            }
        } else {
            let need = div_round(self.ntl_abs(asset, p.szi), lev.value() as i128, Rounding::Ceil);
            if self.iso_equity(p) + delta.raw() < need {
                return err(ERR_ISO_REMOVE);
            }
        }
        let st = &mut self.accts[i];
        let k = st.positions.binary_search_by_key(&asset, |p| p.asset).expect("pos");
        st.positions[k].iso_cash += delta;
        st.cash -= delta;
        self.touch(i);
        Ok(())
    }

    fn leverage_of(&self, user: &Address, asset: AssetId) -> Leverage {
        match self.account(user) {
            Some(st) => self.lev_in(st, asset),
            None => self.default_lev(asset),
        }
    }

    fn check_order_margin(&self, c: &MarginCheck) -> Result<(), ExecError> {
        self.spec(c.asset)?;
        if c.sz.0 <= 0 || c.px.0 == 0 {
            return err(ERR_FILL);
        }
        if c.reduce_only {
            return Ok(());
        }
        let n = notional(c.px, c.sz, Rounding::Floor).raw();
        let rate = Rate::from_raw(self.params.taker_fee.raw().max(self.params.maker_fee.raw()).max(0));
        let fee = self.fee_for(n, rate);
        let d = Sz::from_raw(c.sz.raw() * c.side.sign() as i128);
        let i = self.idx(&c.user);
        let (_, out) = self.plan_at(i, &c.user, c.asset, d, n, fee, false);
        if self.plan_ok(i, &out) {
            Ok(())
        } else {
            err(ERR_MARGIN)
        }
    }

    fn reduce_only_capacity(&self, user: &Address, asset: AssetId, side: Side) -> Sz {
        let s = self.position(user, asset);
        if (s.0 > 0 && side == Side::Ask) || (s.0 < 0 && side == Side::Bid) {
            s.abs()
        } else {
            Sz::ZERO
        }
    }

    fn apply_fill(&mut self, f: &FillParams) -> Result<FillResult, ExecError> {
        self.spec(f.asset)?;
        if f.sz.0 <= 0 || f.px.0 == 0 || f.taker == f.maker || f.taker == INSURANCE || f.maker == INSURANCE {
            return err(ERR_FILL);
        }
        let n = notional(f.px, f.sz, Rounding::Floor).raw();
        let taker_fee = self.fee_for(n, self.params.taker_fee);
        let maker_fee = self.fee_for(n, self.params.maker_fee);
        let ts = f.taker_side.sign() as i128;
        let td = Sz::from_raw(f.sz.raw() * ts);
        let md = Sz::from_raw(-f.sz.raw() * ts);
        let t_liq = f.liquidation_of == Some(f.taker);
        let m_liq = f.liquidation_of == Some(f.maker);
        let ti = self.idx(&f.taker);
        let mi = self.idx(&f.maker);
        let (t0, tout) = self.plan_at(ti, &f.taker, f.asset, td, n, taker_fee, t_liq);
        let (m0, mout) = self.plan_at(mi, &f.maker, f.asset, md, n, maker_fee, m_liq);
        if (!t_liq && !self.plan_ok(ti, &tout)) || (!m_liq && !self.plan_ok(mi, &mout)) {
            return err(ERR_MARGIN);
        }
        let res = FillResult {
            taker_fee: Usd::from_raw(taker_fee),
            maker_fee: Usd::from_raw(maker_fee),
            taker_closed_pnl: Usd::from_raw(tout.closed_pnl),
            maker_closed_pnl: Usd::from_raw(mout.closed_pnl),
            taker_start_pos: t0,
            maker_start_pos: m0,
        };
        let ti = ti.unwrap_or_else(|| self.get_or_create(f.taker));
        self.commit_at(ti, f.taker, f.asset, tout);
        let mi = mi.unwrap_or_else(|| self.get_or_create(f.maker));
        self.commit_at(mi, f.maker, f.asset, mout);
        self.ledger.fee_pool = Usd::from_raw(self.ledger.fee_pool.raw() + taker_fee + maker_fee);
        self.total += taker_fee + maker_fee;
        self.dirty_insurance = true;
        let m = &mut self.markets[f.asset as usize];
        m.day_ntl_vlm = Usd::from_raw(m.day_ntl_vlm.raw() + n);
        Ok(res)
    }

    fn position(&self, user: &Address, asset: AssetId) -> Sz {
        self.account(user).map(|a| a.szi(asset)).unwrap_or(Sz::ZERO)
    }

    fn set_prices(&mut self, asset: AssetId, oracle_px: Px, mark_px: Px, mid_px: Option<Px>, now: TimestampMs) {
        let Some(m) = self.markets.get_mut(asset as usize) else { return };
        assert!(oracle_px.0 > 0 && mark_px.0 > 0, "prices must be positive");
        let day = now / DAY_MS;
        if day > m.day {
            m.prev_day_px = m.mark_px;
            m.day_ntl_vlm = Usd::ZERO;
            m.day = day;
        }
        let moved = m.mark_px != mark_px;
        m.oracle_px = oracle_px;
        m.mark_px = mark_px;
        m.mid_px = mid_px;
        self.dirty_markets[asset as usize] = true;
        if moved {
            self.on_mark_moved(asset);
        }
        self.refresh_pending();
    }

    fn on_block_funding(&mut self, now: TimestampMs) -> Vec<Event> {
        let mut ev = Vec::new();
        let interval = self.params.funding_interval_ms.max(1);
        for a in 0..self.markets.len() as AssetId {
            let slot = now / interval;
            if slot > self.markets[a as usize].last_funding_slot {
                self.settle_funding(a, now, &mut ev);
                self.markets[a as usize].last_funding_slot = slot;
            }
            let ss = now / PREMIUM_SAMPLE_MS;
            let m = &mut self.markets[a as usize];
            if ss > m.last_sample_slot {
                let p = px_rel_diff(m.mark_px, m.oracle_px, Rounding::TowardZero);
                m.premium_sum += p.raw();
                m.premium_samples += 1;
                m.last_premium = p;
                m.last_sample_slot = ss;
                self.dirty_markets[a as usize] = true;
            }
        }
        ev
    }

    fn liquidation_targets(&self) -> Vec<LiquidationTarget> {
        self.liquidation_targets_impl()
    }

    fn backstop_and_adl(&mut self, user: Address, now: TimestampMs) -> Vec<Event> {
        let mut ev = Vec::new();
        let mut work = VecDeque::new();
        work.push_back(user);
        let mut guard = 0usize;
        while let Some(u) = work.pop_front() {
            guard += 1;
            assert!(guard <= 1 + 4 * self.accts.len(), "ADL cascade did not terminate");
            self.resolve(u, now, &mut ev, &mut work);
        }
        self.refresh_pending();
        ev
    }

    fn account_view(&self, user: &Address) -> AccountView {
        self.account_view_impl(user)
    }

    fn market_ctx(&self, asset: AssetId) -> MarketCtx {
        self.market_ctx_impl(asset)
    }

    fn insurance_fund(&self) -> Usd {
        let ins = &self.accts[0];
        let mut e = ins.cash.raw();
        for p in &ins.positions {
            e += self.value(p.asset, p.szi);
        }
        Usd::from_raw(e)
    }

    fn total_collateral(&self) -> Usd {
        Usd::from_raw(self.total)
    }

    fn check_invariants(&self, flows: &ExternalFlows, height: Height) -> Result<(), InvariantViolation> {
        self.check_invariants_impl(flows, height)
    }

    fn take_dirty(&mut self) -> Vec<(StateKey, Hash32)> {
        let mut out = Vec::new();
        let mut list = std::mem::take(&mut self.dirty_list);
        for i in &list {
            self.meta[*i as usize].dirty = false;
        }
        list.sort_by(|a, b| self.accts[*a as usize].address.cmp(&self.accts[*b as usize].address));
        list.dedup();
        for i in list {
            if i == 0 {
                // never report the insurance pseudo-account as an Account key
                self.dirty_insurance = true;
                continue;
            }
            let st = &self.accts[i as usize];
            out.push((StateKey::Account(st.address), hash_borsh(st)));
        }
        for a in 0..self.markets.len() {
            if self.dirty_markets[a] {
                self.dirty_markets[a] = false;
                out.push((StateKey::Market(a as AssetId), hash_borsh(&self.markets[a])));
            }
        }
        if self.dirty_insurance {
            self.dirty_insurance = false;
            out.push((StateKey::Insurance, hash_borsh(&(&self.accts[0], &self.ledger))));
        }
        out
    }

    fn encode(&self) -> Vec<u8> {
        let mut accounts = self.accts.clone();
        accounts.sort_by(|a, b| a.address.cmp(&b.address));
        let snap = ChSnapshot {
            version: SNAPSHOT_VERSION,
            specs: self.specs.clone(),
            params: self.params.clone(),
            spot_tokens: self.spot_tokens.clone(),
            markets: self.markets.clone(),
            accounts,
            ledger: self.ledger.clone(),
        };
        borsh::to_vec(&snap).expect("borsh")
    }
}

impl Clearinghouse {
    /// Every entity hash (as `take_dirty` would report them after a full
    /// rebuild). Used by the state crate after `decode` / for audits.
    pub fn all_entity_hashes(&self) -> Vec<(StateKey, Hash32)> {
        let mut out: Vec<(StateKey, Hash32)> = self.accts[1..]
            .iter()
            .map(|st| (StateKey::Account(st.address), hash_borsh(st)))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        for (a, m) in self.markets.iter().enumerate() {
            out.push((StateKey::Market(a as AssetId), hash_borsh(m)));
        }
        out.push((StateKey::Insurance, hash_borsh(&(&self.accts[0], &self.ledger))));
        out
    }
}

#[cfg(test)]
mod tests;
