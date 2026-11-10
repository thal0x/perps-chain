//! The embedded application behind the simulator, plus the observation
//! ("world view") the agents get from it.

use interfaces::*;
use std::sync::Arc;

/// `interfaces::Application` plus the extra hooks the simulator uses.
pub trait SimApp: Application {
    /// Full invariant check of the current state (AC4).
    fn check_invariants(&self) -> Result<(), String> {
        Ok(())
    }
    /// Accounts with negative equity after the block's liquidation pass.
    fn negative_equity_accounts(&self) -> Vec<Address> {
        Vec::new()
    }
    /// Pre-seed signature recovery for a tx whose decoded action and signer
    /// are known (generation only; a pure function of the tx bytes, so it
    /// cannot change results). Default: ignored.
    fn prime_recovered(&self, _hash: TxHash, _action: Action, _signer: Address) {}
    /// Whether `query()` reflects real state (agents use it for feedback).
    fn has_feedback(&self) -> bool {
        true
    }
}

/// Builds an embedded application from a genesis.
pub type AppFactory = Arc<dyn Fn(&Genesis, AppOptions) -> Box<dyn SimApp> + Send + Sync>;

#[derive(Clone, Copy, Debug, Default)]
pub struct AppOptions {
    /// Let the app run its own invariant checks every block (in addition to
    /// the replay's explicit checks).
    pub internal_invariants: bool,
}

// ------------------------------------------------------------- state crate

impl SimApp for state::HlChain {
    fn check_invariants(&self) -> Result<(), String> {
        state::HlChain::check_invariants(self).map_err(|e| e.to_string())
    }
    fn negative_equity_accounts(&self) -> Vec<Address> {
        self.clearinghouse().negative_equity_accounts()
    }
    fn prime_recovered(&self, hash: TxHash, action: Action, signer: Address) {
        self.sig_cache().insert(hash, Arc::new(Ok((action, signer))));
    }
}

/// Factory for the production chain (`state::HlChain`, real engines and
/// HL signatures).
pub fn state_factory() -> AppFactory {
    Arc::new(|g: &Genesis, o: AppOptions| {
        let mut cfg = state::ChainConfig::default();
        cfg.invariants = if o.internal_invariants { state::InvariantMode::EveryBlock } else { state::InvariantMode::Never };
        cfg.panic_on_invariant_violation = false;
        cfg.sig_cache_capacity = 2_000_000;
        Box::new(state::HlChain::new_hl(g.clone(), cfg)) as Box<dyn SimApp>
    })
}

// ---------------------------------------------------------------- null app

/// Minimal application: root = blake3 hash chain over (height, ts, payload),
/// no state. Used by tests and `--app null` (pure generator benchmarks).
pub struct NullApp {
    genesis: Genesis,
    height: Height,
    time: TimestampMs,
    roots: Vec<Hash32>,
    q: NullQuery,
}

struct NullQuery {
    markets: Vec<MarketSpec>,
    vset: ValidatorSet,
    height: Height,
    time: TimestampMs,
}

impl NullApp {
    pub fn new(g: &Genesis) -> Self {
        NullApp {
            genesis: g.clone(),
            height: 0,
            time: g.genesis_time_ms,
            roots: vec![hash_borsh(g)],
            q: NullQuery { markets: g.markets.clone(), vset: g.validators.clone(), height: 0, time: g.genesis_time_ms },
        }
    }
}

pub fn null_factory() -> AppFactory {
    Arc::new(|g: &Genesis, _| Box::new(NullApp::new(g)) as Box<dyn SimApp>)
}

impl Application for NullApp {
    fn genesis(&self) -> &Genesis {
        &self.genesis
    }
    fn executed_height(&self) -> Height {
        self.height
    }
    fn root_at(&self, h: Height) -> Option<Hash32> {
        self.roots.get(h as usize).copied()
    }
    fn execute_block(&mut self, ctx: &BlockContext, txs: &[Tx]) -> BlockOutcome {
        assert_eq!(ctx.height, self.height + 1);
        self.height = ctx.height;
        self.time = self.time.max(ctx.timestamp_ms);
        let prev = *self.roots.last().unwrap();
        let root = hash_borsh(&(prev, ctx.height, ctx.timestamp_ms, Block::payload_hash(txs)));
        self.roots.push(root);
        self.q.height = self.height;
        self.q.time = self.time;
        let mut order_actions = 0;
        let mut receipts = Vec::new();
        for t in txs {
            if let Tx::User(u) = t {
                order_actions += crate::sign::order_action_count_bytes(&u.action_bytes);
                receipts.push(TxReceipt { tx_hash: t.hash(), user: Address::ZERO, result: Ok(ActionResponse::Default) });
            }
        }
        BlockOutcome { height: self.height, state_root: root, receipts, events: vec![], order_actions }
    }
    fn check_tx(&self, tx: &Tx) -> Result<Address, AdmissionError> {
        match tx {
            Tx::User(u) => hlwire::recover_signer(u, self.genesis.is_mainnet).map_err(|_| AdmissionError::BadSignature),
            Tx::Validator(v) => hlwire::recover_validator_tx(v, self.genesis.chain_id).map_err(|_| AdmissionError::BadSignature),
        }
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot { height: self.height, state_root: *self.roots.last().unwrap(), data: vec![] }
    }
    fn restore(&mut self, _s: &Snapshot) -> Result<Hash32, String> {
        Err("NullApp cannot restore".into())
    }
    fn query(&self) -> &dyn StateQuery {
        &self.q
    }
}

impl SimApp for NullApp {
    fn has_feedback(&self) -> bool {
        false
    }
}

impl StateQuery for NullQuery {
    fn markets(&self) -> Vec<MarketSpec> {
        self.markets.clone()
    }
    fn market_ctx(&self, asset: AssetId) -> MarketCtx {
        let px = self.markets.get(asset as usize).map(|m| m.initial_px).unwrap_or(Px(1));
        MarketCtx {
            asset,
            oracle_px: px,
            mark_px: px,
            mid_px: None,
            funding: Rate::ZERO,
            premium: Rate::ZERO,
            open_interest: Sz::ZERO,
            day_ntl_vlm: Usd::ZERO,
            prev_day_px: px,
        }
    }
    fn mid(&self, _asset: AssetId) -> Option<Px> {
        None
    }
    fn l2_book(&self, _asset: AssetId, _depth: usize) -> (Vec<(Px, Sz, u32)>, Vec<(Px, Sz, u32)>) {
        (vec![], vec![])
    }
    fn account(&self, _user: &Address) -> AccountView {
        AccountView::default()
    }
    fn open_orders(&self, _user: &Address) -> Vec<RestingOrderView> {
        vec![]
    }
    fn order_by_oid(&self, _user: &Address, _oid: Oid) -> Option<RestingOrderView> {
        None
    }
    fn order_by_cloid(&self, _user: &Address, _cloid: &Cloid) -> Option<RestingOrderView> {
        None
    }
    fn principal_of(&self, signer: &Address) -> Address {
        *signer
    }
    fn block_time(&self) -> TimestampMs {
        self.time
    }
    fn height(&self) -> Height {
        self.height
    }
    fn pending_withdrawals(&self) -> Vec<PendingWithdrawal> {
        vec![]
    }
    fn processed_deposit(&self, _deposit_id: u64) -> bool {
        false
    }
    fn validator_set(&self) -> ValidatorSet {
        self.vset.clone()
    }
}

// ------------------------------------------------------------- observation

/// Market summary seen by agents.
#[derive(Clone, Debug, Default)]
pub struct MarketObs {
    pub mark: Option<Px>,
    pub oracle: Option<Px>,
    pub best_bid: Option<Px>,
    pub best_ask: Option<Px>,
}

/// Cached per-account summary (refreshed for a rotating subset each block).
#[derive(Clone, Debug, Default)]
pub struct AccountObs {
    pub height: Height,
    pub account_value: i128,
    pub maintenance: i128,
    pub withdrawable: i128,
    /// (asset, szi raw, |position value| raw, is_cross)
    pub positions: Vec<(AssetId, i64, i128, bool)>,
    pub spot: Vec<(u32, i128)>,
}

impl AccountObs {
    pub fn position(&self, asset: AssetId) -> i64 {
        self.positions.iter().find(|p| p.0 == asset).map(|p| p.1).unwrap_or(0)
    }
    pub fn from_view(v: &AccountView, height: Height) -> Self {
        AccountObs {
            height,
            account_value: v.margin_summary.account_value.0 as i128,
            maintenance: v.cross_maintenance_margin_used.0 as i128,
            withdrawable: v.withdrawable.0 as i128,
            positions: v
                .positions
                .iter()
                .filter(|p| p.szi.0 != 0)
                .map(|p| (p.asset, p.szi.0, (p.position_value.0 as i128).abs(), p.leverage.is_cross()))
                .collect(),
            spot: v.spot_balances.iter().map(|(t, a)| (*t, a.0 as i128)).collect(),
        }
    }
}

/// What the agents know about the chain. Built only from `StateQuery`.
#[derive(Clone, Debug, Default)]
pub struct World {
    pub feedback: bool,
    pub height: Height,
    pub markets: Vec<MarketObs>,
    pub accounts: Vec<Option<AccountObs>>,
    /// Open orders of accounts whose orders were refreshed after the last block.
    pub open_orders: Vec<Option<Vec<RestingOrderView>>>,
    pub pending_withdrawals: Vec<PendingWithdrawal>,
}

impl World {
    pub fn new(n_markets: usize, n_accounts: usize, feedback: bool) -> Self {
        World {
            feedback,
            height: 0,
            markets: vec![MarketObs::default(); n_markets],
            accounts: vec![None; n_accounts],
            open_orders: vec![None; n_accounts],
            pending_withdrawals: Vec::new(),
        }
    }

    /// Refresh market summaries, the given accounts and open-order lists.
    pub fn refresh(&mut self, q: &dyn StateQuery, accounts: &[(usize, Address)], orders: &[(usize, Address)]) {
        if !self.feedback {
            return;
        }
        self.height = q.height();
        for (i, m) in self.markets.iter_mut().enumerate() {
            let c = q.market_ctx(i as AssetId);
            let (b, a) = q.l2_book(i as AssetId, 1);
            *m = MarketObs {
                mark: Some(c.mark_px),
                oracle: Some(c.oracle_px),
                best_bid: b.first().map(|x| x.0),
                best_ask: a.first().map(|x| x.0),
            };
        }
        for (i, a) in accounts {
            self.accounts[*i] = Some(AccountObs::from_view(&q.account(a), self.height));
        }
        for o in self.open_orders.iter_mut() {
            *o = None;
        }
        for (i, a) in orders {
            self.open_orders[*i] = Some(q.open_orders(a));
        }
        self.pending_withdrawals = q.pending_withdrawals();
    }
}
