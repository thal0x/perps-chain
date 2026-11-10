//! `Chain`: the replicated state machine.

use crate::config::*;
use crate::engines::Engines;
use crate::errors as err_msg;
use crate::hook::Hook;
use crate::meta::*;
use crate::sigcache::{Recovered, SigCache};
use crate::views::*;
use borsh::{BorshDeserialize, BorshSerialize};
use interfaces::*;
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

/// Outcome of placing one order through the book (internal).
pub(crate) struct Placed {
    pub out: PlaceOutcome,
    pub margin_stopped: bool,
    pub reduce_stopped: bool,
}

pub struct Chain<E: Engines, V: TxVerifier> {
    pub(crate) genesis: Genesis,
    pub(crate) cfg: ChainConfig,
    pub(crate) verifier: Arc<V>,
    pub(crate) cache: Arc<SigCache>,
    pub(crate) core: Core,
    pub(crate) ch: E::Ch,
    pub(crate) books: Vec<E::Book>,
    pub(crate) trigs: Vec<E::Trig>,
    pub(crate) oracle: E::Oracle,
    // ---- commitment ----
    pub(crate) ch_hashes: BTreeMap<StateKey, Hash32>,
    pub(crate) own_hashes: BTreeMap<StateKey, Hash32>,
    pub(crate) dirty_users: BTreeSet<Address>,
    pub(crate) dirty_books: Vec<bool>,
    pub(crate) bridge_dirty: bool,
    pub(crate) roots: VecDeque<(Height, Hash32)>,
    // ---- per-block scratch ----
    pub(crate) events: Vec<Event>,
    pub(crate) cur_tx: TxHash,
    pub(crate) violations: Vec<InvariantViolation>,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct SnapshotData {
    core: Core,
    ch: Vec<u8>,
    books: Vec<Vec<u8>>,
    trigs: Vec<Vec<u8>>,
}

fn is_user_signed(a: &Action) -> bool {
    matches!(
        a,
        Action::UsdSend { .. }
            | Action::SpotSend { .. }
            | Action::UsdClassTransfer { .. }
            | Action::Withdraw3 { .. }
            | Action::ApproveAgent { .. }
    )
}

/// The nonce/time field embedded in user-signed actions.
fn embedded_nonce(a: &Action) -> Option<u64> {
    match a {
        Action::UsdSend { time, .. } | Action::SpotSend { time, .. } | Action::Withdraw3 { time, .. } => Some(*time),
        Action::UsdClassTransfer { nonce, .. } | Action::ApproveAgent { nonce, .. } => Some(*nonce),
        _ => None,
    }
}

fn median3(a: Px, b: Px, c: Px) -> Px {
    let mut v = [a, b, c];
    v.sort();
    v[1]
}

impl<E: Engines, V: TxVerifier> Chain<E, V> {
    pub fn new(genesis: Genesis, verifier: Arc<V>, cfg: ChainConfig) -> Self {
        let n = genesis.markets.len();
        for (i, m) in genesis.markets.iter().enumerate() {
            assert_eq!(m.asset as usize, i, "genesis markets must be index-ordered");
        }
        let ch = E::new_clearinghouse(&genesis);
        let books = genesis.markets.iter().map(E::new_book).collect();
        let trigs = genesis.markets.iter().map(E::new_triggers).collect();
        let oracle = E::new_oracle(&genesis);
        let genesis_total = E::genesis_total(&genesis);
        let core = Core {
            height: 0,
            time: genesis.genesis_time_ms,
            next_oid: 1,
            next_tid: 1,
            flows: ExternalFlows { deposits: Usd::ZERO, withdrawals: Usd::ZERO, genesis: genesis_total },
            users: BTreeMap::new(),
            agents: BTreeMap::new(),
            scheduled: BTreeSet::new(),
            validator_nonces: BTreeMap::new(),
            oracle_subs: BTreeMap::new(),
            oracle_px: genesis.markets.iter().map(|m| m.initial_px).collect(),
            mark_px: genesis.markets.iter().map(|m| m.initial_px).collect(),
            bridge: BridgeState { next_withdrawal_id: 1, ..Default::default() },
            tpsl_parents: BTreeMap::new(),
        };
        let cache = Arc::new(SigCache::new(cfg.sig_cache_capacity));
        let mut c = Self {
            genesis,
            cfg,
            verifier,
            cache,
            core,
            ch,
            books,
            trigs,
            oracle,
            ch_hashes: BTreeMap::new(),
            own_hashes: BTreeMap::new(),
            dirty_users: BTreeSet::new(),
            dirty_books: vec![true; n],
            bridge_dirty: true,
            roots: VecDeque::new(),
            events: Vec::new(),
            cur_tx: Hash32::ZERO,
            violations: Vec::new(),
        };
        for a in 0..n as AssetId {
            let (o, m, mid) = (c.core.oracle_px[a as usize], c.core.mark_px[a as usize], None);
            c.ch.set_prices(a, o, m, mid, c.core.time);
        }
        let root = c.commit();
        c.roots.push_back((0, root));
        c
    }

    // ------------------------------------------------------------ accessors

    pub fn genesis_ref(&self) -> &Genesis {
        &self.genesis
    }
    pub fn clearinghouse(&self) -> &E::Ch {
        &self.ch
    }
    pub fn book(&self, asset: AssetId) -> &E::Book {
        &self.books[asset as usize]
    }
    pub fn triggers(&self, asset: AssetId) -> &E::Trig {
        &self.trigs[asset as usize]
    }
    pub fn flows(&self) -> ExternalFlows {
        self.core.flows
    }
    pub fn current_root(&self) -> Hash32 {
        self.roots.back().map(|r| r.1).unwrap_or(Hash32::ZERO)
    }
    pub fn invariant_violations(&self) -> &[InvariantViolation] {
        &self.violations
    }
    pub fn user_meta(&self, a: &Address) -> Option<&UserMeta> {
        self.core.users.get(a)
    }
    pub fn oracle_px(&self, asset: AssetId) -> Px {
        self.core.oracle_px[asset as usize]
    }
    pub fn mark_px(&self, asset: AssetId) -> Px {
        self.core.mark_px[asset as usize]
    }
    pub fn next_oid(&self) -> Oid {
        self.core.next_oid
    }
    /// Shared signature cache (give it to whatever calls `check_tx`).
    pub fn sig_cache(&self) -> Arc<SigCache> {
        self.cache.clone()
    }
    /// All committed (key, hash) entries: (state-owned, clearinghouse-owned).
    pub fn entity_hashes(&self) -> (Vec<(StateKey, Hash32)>, Vec<(StateKey, Hash32)>) {
        (
            self.own_hashes.iter().map(|(k, v)| (*k, *v)).collect(),
            self.ch_hashes.iter().map(|(k, v)| (*k, *v)).collect(),
        )
    }
    pub fn bridge_state(&self) -> &BridgeState {
        &self.core.bridge
    }

    /// Run all invariant checks now.
    pub fn check_invariants(&self) -> Result<(), InvariantViolation> {
        let h = self.core.height;
        self.ch.check_invariants(&self.core.flows, h)?;
        for (i, b) in self.books.iter().enumerate() {
            if let (Some(bid), Some(ask)) = (b.best_bid(), b.best_ask()) {
                if bid >= ask {
                    return Err(InvariantViolation { height: h, what: format!("book {i} crossed: {bid} >= {ask}") });
                }
            }
        }
        let mut pending = Usd::ZERO;
        for w in self.core.bridge.pending_withdrawals.values() {
            if w.amount.0 <= 0 {
                return Err(InvariantViolation { height: h, what: format!("withdrawal {} non-positive", w.id) });
            }
            pending += w.amount;
        }
        if pending > self.core.flows.withdrawals {
            return Err(InvariantViolation { height: h, what: "pending withdrawals exceed total withdrawn".into() });
        }
        Ok(())
    }

    fn on_violation(&mut self, v: InvariantViolation) {
        if self.cfg.panic_on_invariant_violation {
            panic!("{v}");
        }
        self.violations.push(v);
    }

    // ------------------------------------------------------------ recovery

    fn recover_user(&self, u: &UserTx) -> Recovered {
        let action = self.verifier.decode(u).map_err(|e| format!("Failed to decode action: {e}"))?;
        let signer = self.verifier.recover_signer(u, &action).map_err(|_| err_msg::INVALID_SIGNATURE.to_string())?;
        Ok((action, signer))
    }

    fn recover_cached(&self, hash: &TxHash, u: &UserTx) -> Arc<Recovered> {
        if let Some(r) = self.cache.get(hash) {
            return r;
        }
        let r = Arc::new(self.recover_user(u));
        self.cache.insert(*hash, r.clone());
        r
    }

    // --------------------------------------------------------- dirty marks

    #[inline]
    pub(crate) fn touch_book(&mut self, asset: AssetId) {
        self.dirty_books[asset as usize] = true;
    }
    #[inline]
    fn meta_mut(&mut self, a: Address) -> &mut UserMeta {
        self.dirty_users.insert(a);
        self.core.users.entry(a).or_default()
    }

    // ===================================================== block execution

    pub fn execute(&mut self, ctx: &BlockContext, txs: &[Tx]) -> BlockOutcome {
        assert_eq!(ctx.height, self.core.height + 1, "blocks must be executed in height order");
        self.events.clear();
        self.core.height = ctx.height;
        // Block time is monotone (consensus enforces it; be defensive).
        self.core.time = self.core.time.max(ctx.timestamp_ms);
        let now = self.core.time;

        // ---- 1. begin
        self.cur_tx = Hash32::ZERO;
        self.fire_scheduled_cancels(now);
        let fe = self.ch.on_block_funding(now);
        self.events.extend(fe);

        // ---- 2. signature pre-pass (parallel, pure)
        let chain_id = self.genesis.chain_id;
        let pre: Vec<Pre> = txs
            .par_iter()
            .map(|tx| match tx {
                Tx::User(u) => {
                    let h = tx.hash();
                    let r = match self.cache.take(&h) {
                        Some(r) => r,
                        None => Arc::new(self.recover_user(u)),
                    };
                    Pre::User(h, r)
                }
                Tx::Validator(v) => Pre::Validator(self.verifier.recover_validator(v, chain_id).ok()),
            })
            .collect();

        // ---- 3. transactions in payload order
        let mut receipts = Vec::new();
        let mut order_actions = 0u64;
        for (tx, p) in txs.iter().zip(pre) {
            match (tx, p) {
                (Tx::User(u), Pre::User(h, r)) => {
                    self.cur_tx = h;
                    let (receipt, n) = self.exec_user_tx(u, h, &r);
                    order_actions += n;
                    receipts.push(receipt);
                }
                (Tx::Validator(v), Pre::Validator(signer)) => {
                    self.cur_tx = Hash32::ZERO;
                    self.exec_validator_tx(v, signer);
                }
                _ => unreachable!(),
            }
        }
        self.cur_tx = Hash32::ZERO;

        // ---- 4. end
        self.update_prices(now);
        self.activate_triggers(now);
        self.run_liquidations(now);
        let run_inv = match self.cfg.invariants {
            InvariantMode::Never => false,
            InvariantMode::EveryBlock => true,
            InvariantMode::EveryN(n) => n > 0 && ctx.height.is_multiple_of(n),
        };
        if run_inv {
            if let Err(v) = E::post_liquidation_check(&self.ch, ctx.height) {
                self.on_violation(v);
            }
        }
        for (a, d) in self.dirty_books.iter().enumerate() {
            if *d {
                self.events.push(Event::BookChanged { asset: a as AssetId });
            }
        }
        if run_inv {
            if let Err(v) = self.check_invariants() {
                self.on_violation(v);
            }
        }
        let root = self.commit();
        self.roots.push_back((ctx.height, root));
        while self.roots.len() > self.cfg.root_window.max(1) {
            self.roots.pop_front();
        }
        BlockOutcome {
            height: ctx.height,
            state_root: root,
            receipts,
            events: std::mem::take(&mut self.events),
            order_actions,
        }
    }

    // ------------------------------------------------------------ user txs

    fn user_exists(&self, a: &Address) -> bool {
        // Nonce records alone do not make an account (e.g. a revoked agent).
        if self.core.users.get(a).is_some_and(|m| {
            m.known || m.unnamed_agent.is_some() || !m.named_agents.is_empty() || m.referrer.is_some() || m.scheduled_cancel.is_some()
        }) {
            return true;
        }
        let v = self.ch.account_view(a);
        v.margin_summary.account_value.0 != 0 || !v.positions.is_empty() || !v.spot_balances.is_empty()
    }

    /// Validates and applies one user tx. Returns (receipt, order actions).
    fn exec_user_tx(&mut self, u: &UserTx, hash: TxHash, r: &Recovered) -> (TxReceipt, u64) {
        let err = |user: Address, m: String| (TxReceipt { tx_hash: hash, user, result: Err(m) }, 0);
        let (action, signer) = match r {
            Ok((a, s)) => (a, *s),
            Err(e) => return err(Address::ZERO, e.clone()),
        };
        let now = self.core.time;
        if u.vault_address.is_some() {
            return err(signer, err_msg::VAULT_UNSUPPORTED.into());
        }
        if let Some(e) = u.expires_after {
            if e < now {
                return err(signer, err_msg::EXPIRED.into());
            }
        }
        if let Some(n) = embedded_nonce(action) {
            if n != u.nonce {
                return err(signer, err_msg::NONCE_MISMATCH.into());
            }
        }
        if let Some(m) = self.core.users.get(&signer) {
            if let Err(e) = m.check_nonce(u.nonce, now) {
                return err(signer, e);
            }
        } else if let Err(e) = UserMeta::default().check_nonce(u.nonce, now) {
            return err(signer, e);
        }
        let user = if is_user_signed(action) { signer } else { *self.core.agents.get(&signer).unwrap_or(&signer) };
        if !self.user_exists(&user) {
            return err(user, err_msg::user_does_not_exist(&signer));
        }
        if !self.core.users.get(&user).is_some_and(|m| m.known) {
            self.meta_mut(user).known = true;
        }
        self.meta_mut(signer).use_nonce(u.nonce);
        let n = action.order_action_count() as u64;
        let result = self.exec_action(user, action, u.nonce);
        (TxReceipt { tx_hash: hash, user, result }, n)
    }

    fn exec_action(&mut self, user: Address, action: &Action, nonce: u64) -> Result<ActionResponse, String> {
        let now = self.core.time;
        match action {
            Action::Order { orders, grouping } => Ok(ActionResponse::Order(self.exec_order_batch(user, orders, *grouping))),
            Action::Cancel { cancels } => {
                let st = cancels.iter().map(|(a, oid)| self.cancel_by_oid(user, *a, *oid)).collect();
                Ok(ActionResponse::Cancel(st))
            }
            Action::CancelByCloid { cancels } => {
                let st = cancels.iter().map(|(a, c)| self.cancel_by_cloid(user, *a, c)).collect();
                Ok(ActionResponse::Cancel(st))
            }
            Action::Modify { oid, order } => match self.modify(user, *oid, order) {
                OrderStatusResp::Error(e) => Err(e),
                _ => Ok(ActionResponse::Default),
            },
            Action::BatchModify { modifies } => {
                let st = modifies.iter().map(|(o, req)| self.modify(user, *o, req)).collect();
                Ok(ActionResponse::Order(st))
            }
            Action::ScheduleCancel { time } => {
                if let Some(t) = time {
                    if *t < now.saturating_add(SCHEDULE_CANCEL_MIN_DELAY_MS) {
                        return Err(err_msg::SCHEDULE_TOO_EARLY.into());
                    }
                }
                let old = self.meta_mut(user).scheduled_cancel.take();
                if let Some(o) = old {
                    self.core.scheduled.remove(&(o, user));
                }
                if let Some(t) = time {
                    self.meta_mut(user).scheduled_cancel = Some(*t);
                    self.core.scheduled.insert((*t, user));
                }
                Ok(ActionResponse::Default)
            }
            Action::UpdateLeverage { asset, is_cross, leverage } => {
                let spec = self.spec(*asset).ok_or(err_msg::INVALID_ASSET)?;
                if *leverage == 0 || *leverage > spec.max_leverage {
                    return Err(err_msg::INVALID_LEVERAGE.into());
                }
                self.ch.update_leverage(user, *asset, *is_cross, *leverage).map_err(|e| e.0)?;
                Ok(ActionResponse::Default)
            }
            Action::UpdateIsolatedMargin { asset, ntli, .. } => {
                self.spec(*asset).ok_or(err_msg::INVALID_ASSET)?;
                self.ch.update_isolated_margin(user, *asset, *ntli).map_err(|e| e.0)?;
                Ok(ActionResponse::Default)
            }
            Action::UsdSend { destination, amount, .. } => {
                if amount.0 <= 0 {
                    return Err(err_msg::INVALID_AMOUNT.into());
                }
                self.ch.usd_send(user, *destination, *amount).map_err(|e| e.0)?;
                let delta = LedgerDelta::InternalTransfer {
                    usdc: *amount,
                    user,
                    destination: *destination,
                    fee: Usd::ZERO,
                };
                self.ledger2(user, *destination, delta);
                Ok(ActionResponse::Default)
            }
            Action::SpotSend { destination, token, amount, .. } => {
                if amount.0 <= 0 {
                    return Err(err_msg::INVALID_AMOUNT.into());
                }
                let idx = self.parse_spot_token(token).ok_or(err_msg::INVALID_TOKEN)?;
                self.ch.spot_send(user, *destination, idx, *amount).map_err(|e| e.0)?;
                let delta = LedgerDelta::SpotTransfer {
                    token: token.clone(),
                    amount: *amount,
                    user,
                    destination: *destination,
                    fee: Usd::ZERO,
                };
                self.ledger2(user, *destination, delta);
                Ok(ActionResponse::Default)
            }
            Action::UsdClassTransfer { amount, to_perp, .. } => {
                if amount.0 <= 0 {
                    return Err(err_msg::INVALID_AMOUNT.into());
                }
                self.ch.class_transfer(user, *amount, *to_perp).map_err(|e| e.0)?;
                self.ledger(user, LedgerDelta::AccountClassTransfer { usdc: *amount, to_perp: *to_perp });
                Ok(ActionResponse::Default)
            }
            Action::Withdraw3 { destination, amount, .. } => {
                let fee = self.genesis.params.withdraw_fee;
                if *amount <= fee {
                    return Err(err_msg::WITHDRAW_TOO_SMALL.into());
                }
                // Destination receives amount - fee (D18); what leaves the
                // system is engine-defined (real clearinghouse keeps the fee).
                let leaving = E::withdraw(&mut self.ch, user, *amount, fee).map_err(|e| e.0)?;
                self.core.flows.withdrawals += leaving;
                let id = self.core.bridge.next_withdrawal_id;
                self.core.bridge.next_withdrawal_id += 1;
                self.core.bridge.pending_withdrawals.insert(
                    id,
                    PendingWithdrawal {
                        id,
                        user,
                        destination: *destination,
                        amount: *amount - fee,
                        created_at: now,
                        signatures: vec![],
                    },
                );
                self.bridge_dirty = true;
                self.ledger(user, LedgerDelta::Withdraw { usdc: *amount, nonce, fee });
                Ok(ActionResponse::Default)
            }
            Action::ApproveAgent { agent_address, agent_name, .. } => {
                let agent = *agent_address;
                if agent == user {
                    return Err(err_msg::AGENT_IS_SELF.into());
                }
                if let Some(p) = self.core.agents.get(&agent) {
                    if *p != user {
                        return Err(err_msg::AGENT_IN_USE.into());
                    }
                }
                let name = agent_name.as_deref().filter(|n| !n.is_empty()).map(str::to_string);
                // Drop any existing slot of this same agent (re-approval moves it).
                let meta = self.meta_mut(user);
                if meta.unnamed_agent == Some(agent) {
                    meta.unnamed_agent = None;
                }
                meta.named_agents.retain(|_, a| *a != agent);
                let replaced = match name {
                    None => meta.unnamed_agent.replace(agent),
                    Some(n) => meta.named_agents.insert(n, agent),
                };
                if let Some(old) = replaced {
                    self.core.agents.remove(&old);
                }
                self.core.agents.insert(agent, user);
                Ok(ActionResponse::Default)
            }
            Action::SetReferrer { code } => {
                let m = self.meta_mut(user);
                if m.referrer.is_some() {
                    return Err(err_msg::REFERRER_ALREADY_SET.into());
                }
                m.referrer = Some(code.clone());
                Ok(ActionResponse::Default)
            }
            Action::Noop => Ok(ActionResponse::Default),
        }
    }

    fn ledger(&mut self, user: Address, delta: LedgerDelta) {
        let (time, tx_hash) = (self.core.time, self.cur_tx);
        self.events.push(Event::Ledger { user, time, tx_hash, delta });
    }
    fn ledger2(&mut self, a: Address, b: Address, delta: LedgerDelta) {
        if a != b {
            self.ledger(b, delta.clone());
        }
        self.ledger(a, delta);
    }

    /// "NAME:0x<tokenId hex>" (or bare "NAME") -> spot token index.
    fn parse_spot_token(&self, s: &str) -> Option<u32> {
        let (name, id) = match s.split_once(':') {
            Some((n, id)) => (n, Some(id)),
            None => (s, None),
        };
        let tok = self.genesis.spot_tokens.iter().find(|t| t.name == name)?;
        if let Some(id) = id {
            let id = id.strip_prefix("0x").unwrap_or(id);
            let bytes = hex::decode(id).ok()?;
            if bytes.as_slice() != tok.token_id {
                return None;
            }
        }
        Some(tok.index)
    }

    pub(crate) fn spec(&self, asset: AssetId) -> Option<&MarketSpec> {
        self.genesis.markets.get(asset as usize)
    }

    // -------------------------------------------------------------- orders

    /// Static per-order validation (HL tick/lot, min notional, trigger px).
    fn validate_static(&self, req: &OrderRequest, allow_zero_sz: bool) -> Result<(), String> {
        let spec = self.spec(req.asset).ok_or(err_msg::INVALID_ASSET)?;
        if !spec.is_valid_px(req.limit_px) {
            return Err(err_msg::INVALID_PRICE.into());
        }
        if let OrderKind::Trigger { trigger_px, .. } = req.kind {
            if !spec.is_valid_px(trigger_px) {
                return Err(err_msg::INVALID_TPSL_PRICE.into());
            }
        }
        if allow_zero_sz && req.sz.0 == 0 {
            return Ok(());
        }
        if !spec.is_valid_sz(req.sz) {
            return Err(err_msg::INVALID_SIZE.into());
        }
        if !req.reduce_only && notional(req.limit_px, req.sz, Rounding::Floor).0 < 10 * USD_SCALE as i64 {
            return Err(err_msg::MIN_VALUE.into());
        }
        Ok(())
    }

    /// Stateful admission checks for a limit order; returns the size to place.
    fn validate_limit(&self, user: Address, req: &OrderRequest) -> Result<Sz, String> {
        let side = Side::from_is_buy(req.is_buy);
        if req.reduce_only {
            let cap = self.ch.reduce_only_capacity(&user, req.asset, side);
            if cap.0 <= 0 {
                return Err(err_msg::REDUCE_ONLY_INCREASE.into());
            }
            Ok(req.sz.min(cap))
        } else {
            let mc = MarginCheck { user, asset: req.asset, side, px: req.limit_px, sz: req.sz, reduce_only: false };
            self.ch.check_order_margin(&mc).map_err(|_| err_msg::INSUFFICIENT_MARGIN.to_string())?;
            Ok(req.sz)
        }
    }

    fn alloc_oid(&mut self) -> Oid {
        let o = self.core.next_oid;
        self.core.next_oid += 1;
        o
    }

    fn exec_order_batch(&mut self, user: Address, orders: &[OrderRequest], grouping: Grouping) -> Vec<OrderStatusResp> {
        let mut out = Vec::with_capacity(orders.len());
        match grouping {
            Grouping::Na => {
                for req in orders {
                    out.push(self.place_request(user, req, None, false));
                }
            }
            Grouping::PositionTpsl => {
                for req in orders {
                    if !matches!(req.kind, OrderKind::Trigger { .. }) {
                        out.push(OrderStatusResp::Error(err_msg::POSITION_TPSL_NOT_TRIGGER.into()));
                        continue;
                    }
                    out.push(self.place_request(user, req, None, true));
                }
            }
            Grouping::NormalTpsl => {
                let Some((parent, children)) = orders.split_first() else { return out };
                let ps = self.place_request(user, parent, None, false);
                // Parent still resting => children wait for its fill.
                let parent_wait = match &ps {
                    OrderStatusResp::Resting { oid, .. } => Some(*oid),
                    OrderStatusResp::WaitingForTrigger { oid } => Some(*oid),
                    _ => None,
                };
                let parent_ok = !matches!(ps, OrderStatusResp::Error(_));
                // A triggered parent (trigger order) is reported as resting.
                out.push(ps);
                for c in children {
                    if !parent_ok {
                        out.push(OrderStatusResp::Error(err_msg::TPSL_PARENT_FAILED.into()));
                    } else if !matches!(c.kind, OrderKind::Trigger { .. }) {
                        out.push(OrderStatusResp::Error(err_msg::TPSL_CHILD_NOT_TRIGGER.into()));
                    } else if c.asset != parent.asset {
                        out.push(OrderStatusResp::Error(err_msg::TPSL_ASSET_MISMATCH.into()));
                    } else {
                        let st = self.place_request(user, c, parent_wait, false);
                        out.push(match st {
                            OrderStatusResp::Resting { oid, .. } if parent_wait.is_some() => {
                                OrderStatusResp::WaitingForFill { oid }
                            }
                            OrderStatusResp::Resting { oid, .. } => OrderStatusResp::WaitingForTrigger { oid },
                            s => s,
                        });
                    }
                }
                if let Some(p) = parent_wait {
                    if out[1..].iter().any(|s| matches!(s, OrderStatusResp::WaitingForFill { .. })) {
                        self.core.tpsl_parents.insert(p, parent.asset);
                    }
                }
            }
        }
        out
    }

    /// Place one order request (limit or trigger). Trigger orders are stored
    /// and reported as `Resting` (HL behaviour for standalone triggers).
    fn place_request(&mut self, user: Address, req: &OrderRequest, parent: Option<Oid>, position_tpsl: bool) -> OrderStatusResp {
        if let Err(e) = self.validate_static(req, position_tpsl) {
            return OrderStatusResp::Error(e);
        }
        if let Some(c) = &req.cloid {
            if self.has_cloid(&user, c) {
                return OrderStatusResp::Error(err_msg::DUPLICATE_CLOID.into());
            }
        }
        let side = Side::from_is_buy(req.is_buy);
        let now = self.core.time;
        match req.kind {
            OrderKind::Trigger { trigger_px, is_market, tpsl } => {
                // Reduce-only / margin are checked when the trigger fires.
                let oid = self.alloc_oid();
                let t = TriggerOrder {
                    oid,
                    user,
                    asset: req.asset,
                    side,
                    limit_px: req.limit_px,
                    sz: req.sz,
                    reduce_only: req.reduce_only || position_tpsl,
                    cloid: req.cloid,
                    timestamp: now,
                    trigger_px,
                    is_market,
                    tpsl,
                    parent_oid: parent,
                    is_position_tpsl: position_tpsl,
                };
                self.events.push(Event::OrderUpdate { user, order: trigger_view(&t), status: OrderLifecycle::Open, time: now });
                self.trigs[req.asset as usize].insert(t);
                self.touch_book(req.asset);
                if position_tpsl {
                    OrderStatusResp::WaitingForTrigger { oid }
                } else {
                    OrderStatusResp::Resting { oid, cloid: req.cloid }
                }
            }
            OrderKind::Limit { tif } => {
                let sz = match self.validate_limit(user, req) {
                    Ok(s) => s,
                    Err(e) => return OrderStatusResp::Error(e),
                };
                let oid = self.alloc_oid();
                let o = RestingOrder {
                    oid,
                    user,
                    asset: req.asset,
                    side,
                    limit_px: req.limit_px,
                    sz,
                    orig_sz: sz,
                    reduce_only: req.reduce_only,
                    cloid: req.cloid,
                    timestamp: now,
                    tif,
                };
                let p = self.place_resting(o.clone(), None);
                self.taker_status(&o, &p)
            }
        }
    }

    /// Map a place outcome to an HL status and emit taker order updates.
    fn taker_status(&mut self, o: &RestingOrder, p: &Placed) -> OrderStatusResp {
        let out = &p.out;
        let now = self.core.time;
        if out.alo_rejected {
            let b = self.books[o.asset as usize].best_bid().map(|x| x.to_wire()).unwrap_or_else(|| "None".into());
            let a = self.books[o.asset as usize].best_ask().map(|x| x.to_wire()).unwrap_or_else(|| "None".into());
            return OrderStatusResp::Error(err_msg::post_only_crossed(&b, &a));
        }
        if out.resting_sz.0 > 0 {
            let mut v = resting_view(o);
            v.sz = out.resting_sz;
            self.events.push(Event::OrderUpdate { user: o.user, order: v, status: OrderLifecycle::Open, time: now });
            return OrderStatusResp::Resting { oid: o.oid, cloid: o.cloid };
        }
        if out.filled_sz.0 > 0 {
            let mut v = resting_view(o);
            v.sz = o.sz - out.filled_sz;
            let st = if out.filled_sz == o.sz { OrderLifecycle::Filled } else { OrderLifecycle::Canceled };
            self.events.push(Event::OrderUpdate { user: o.user, order: v, status: st, time: now });
            return OrderStatusResp::Filled {
                total_sz: out.filled_sz,
                avg_px: out.avg_px.unwrap_or(o.limit_px),
                oid: o.oid,
                cloid: o.cloid,
            };
        }
        if p.margin_stopped {
            return OrderStatusResp::Error(err_msg::INSUFFICIENT_MARGIN.into());
        }
        if p.reduce_stopped {
            return OrderStatusResp::Error(err_msg::REDUCE_ONLY_INCREASE.into());
        }
        OrderStatusResp::Error(err_msg::IOC_NO_MATCH.into())
    }

    /// Run an order through the book with the match hook and process the
    /// side effects (maker cancels, TP/SL child release).
    pub(crate) fn place_resting(&mut self, o: RestingOrder, liq: Option<LiquidationInfo>) -> Placed {
        let asset = o.asset;
        let mut hook = Hook {
            ch: &mut self.ch,
            events: &mut self.events,
            next_tid: &mut self.core.next_tid,
            asset,
            time: self.core.time,
            tx_hash: self.cur_tx,
            taker_cloid: o.cloid,
            liquidation: liq,
            taker_margin_stopped: false,
            taker_reduce_stopped: false,
        };
        let taker_oid = o.oid;
        let out = self.books[asset as usize].place(o, &mut hook);
        let (margin_stopped, reduce_stopped) = (hook.taker_margin_stopped, hook.taker_reduce_stopped);
        self.touch_book(asset);
        let now = self.core.time;
        for (m, why) in &out.canceled_makers {
            self.events.push(Event::OrderUpdate { user: m.user, order: resting_view(m), status: *why, time: now });
            self.on_parent_gone(m);
        }
        if !self.core.tpsl_parents.is_empty() {
            for oid in &out.filled_makers {
                self.release_parent(*oid);
            }
            if out.resting_sz.0 == 0 && out.filled_sz.0 > 0 {
                self.release_parent(taker_oid);
            }
        }
        Placed { out, margin_stopped, reduce_stopped }
    }

    fn release_parent(&mut self, oid: Oid) {
        if let Some(asset) = self.core.tpsl_parents.remove(&oid) {
            self.trigs[asset as usize].release_children(oid);
            self.touch_book(asset);
        }
    }

    /// A resting order left the book without filling completely: drop its
    /// waiting TP/SL children if it never filled, else activate them.
    fn on_parent_gone(&mut self, o: &RestingOrder) {
        if !self.core.tpsl_parents.contains_key(&o.oid) {
            return;
        }
        if o.sz == o.orig_sz {
            let asset = self.core.tpsl_parents.remove(&o.oid).expect("present");
            let now = self.core.time;
            for c in self.trigs[asset as usize].remove_children(o.oid) {
                self.events.push(Event::OrderUpdate { user: c.user, order: trigger_view(&c), status: OrderLifecycle::Canceled, time: now });
            }
            self.touch_book(asset);
        } else {
            self.release_parent(o.oid);
        }
    }

    // ------------------------------------------------------------- cancels

    fn cancel_resting(&mut self, asset: AssetId, oid: Oid, why: OrderLifecycle) -> Option<RestingOrder> {
        let o = self.books[asset as usize].cancel(oid)?;
        self.touch_book(asset);
        let now = self.core.time;
        self.events.push(Event::OrderUpdate { user: o.user, order: resting_view(&o), status: why, time: now });
        self.on_parent_gone(&o);
        Some(o)
    }

    fn cancel_trigger(&mut self, asset: AssetId, oid: Oid, why: OrderLifecycle) -> Option<TriggerOrder> {
        let t = self.trigs[asset as usize].remove(oid)?;
        self.touch_book(asset);
        let now = self.core.time;
        self.events.push(Event::OrderUpdate { user: t.user, order: trigger_view(&t), status: why, time: now });
        Some(t)
    }

    fn cancel_by_oid(&mut self, user: Address, asset: AssetId, oid: Oid) -> SimpleStatus {
        if self.spec(asset).is_none() {
            return SimpleStatus::Error(err_msg::CANCEL_MISSING.into());
        }
        let a = asset as usize;
        if self.books[a].get(oid).is_some_and(|o| o.user == user) {
            self.cancel_resting(asset, oid, OrderLifecycle::Canceled);
            return SimpleStatus::Success;
        }
        if self.trigs[a].get(oid).is_some_and(|o| o.user == user) {
            self.cancel_trigger(asset, oid, OrderLifecycle::Canceled);
            return SimpleStatus::Success;
        }
        SimpleStatus::Error(err_msg::CANCEL_MISSING.into())
    }

    /// True if `user` has an open (resting or trigger) order with `cloid`.
    fn has_cloid(&self, user: &Address, cloid: &Cloid) -> bool {
        (0..self.books.len()).any(|a| {
            self.books[a].get_by_cloid(user, cloid).is_some()
                || self.find_trigger_by_cloid(user, a as AssetId, cloid).is_some()
        })
    }

    fn find_trigger_by_cloid(&self, user: &Address, asset: AssetId, cloid: &Cloid) -> Option<Oid> {
        self.trigs[asset as usize].user_orders(user).into_iter().find(|t| t.cloid.as_ref() == Some(cloid)).map(|t| t.oid)
    }

    fn cancel_by_cloid(&mut self, user: Address, asset: AssetId, cloid: &Cloid) -> SimpleStatus {
        if self.spec(asset).is_none() {
            return SimpleStatus::Error(err_msg::CANCEL_MISSING.into());
        }
        if let Some(oid) = self.books[asset as usize].get_by_cloid(&user, cloid).map(|o| o.oid) {
            self.cancel_resting(asset, oid, OrderLifecycle::Canceled);
            return SimpleStatus::Success;
        }
        if let Some(oid) = self.find_trigger_by_cloid(&user, asset, cloid) {
            self.cancel_trigger(asset, oid, OrderLifecycle::Canceled);
            return SimpleStatus::Success;
        }
        SimpleStatus::Error(err_msg::CANCEL_MISSING.into())
    }

    /// Cancel every order of `user` (optionally only in `only_asset`).
    fn cancel_all_of(&mut self, user: Address, only_asset: Option<AssetId>, why: OrderLifecycle) {
        for a in 0..self.books.len() as AssetId {
            if only_asset.is_some_and(|x| x != a) {
                continue;
            }
            let oids: Vec<Oid> = self.books[a as usize].user_orders(&user).iter().map(|o| o.oid).collect();
            for oid in oids {
                self.cancel_resting(a, oid, why);
            }
            let toids: Vec<Oid> = self.trigs[a as usize].user_orders(&user).iter().map(|o| o.oid).collect();
            for oid in toids {
                self.cancel_trigger(a, oid, why);
            }
        }
    }

    fn fire_scheduled_cancels(&mut self, now: TimestampMs) {
        while let Some(&(t, user)) = self.core.scheduled.iter().next() {
            if t > now {
                break;
            }
            self.core.scheduled.remove(&(t, user));
            self.meta_mut(user).scheduled_cancel = None;
            self.cancel_all_of(user, None, OrderLifecycle::ScheduledCancel);
        }
    }

    // -------------------------------------------------------------- modify

    fn modify(&mut self, user: Address, target: OidOrCloid, req: &OrderRequest) -> OrderStatusResp {
        if self.spec(req.asset).is_none() {
            return OrderStatusResp::Error(err_msg::MODIFY_MISSING.into());
        }
        let a = req.asset as usize;
        // Locate the existing order (book first, then triggers).
        enum Found {
            Book(Oid, Option<Cloid>),
            Trig(Oid, Option<Cloid>),
        }
        let found = match target {
            OidOrCloid::Oid(oid) => {
                if let Some(o) = self.books[a].get(oid).filter(|o| o.user == user) {
                    Some(Found::Book(o.oid, o.cloid))
                } else {
                    self.trigs[a].get(oid).filter(|o| o.user == user).map(|t| Found::Trig(t.oid, t.cloid))
                }
            }
            OidOrCloid::Cloid(c) => {
                if let Some(o) = self.books[a].get_by_cloid(&user, &c) {
                    Some(Found::Book(o.oid, o.cloid))
                } else {
                    self.find_trigger_by_cloid(&user, req.asset, &c).map(|oid| Found::Trig(oid, Some(c)))
                }
            }
        };
        let Some(found) = found else { return OrderStatusResp::Error(err_msg::MODIFY_MISSING.into()) };
        // Pre-validate the replacement so a failed modify leaves the original.
        let is_trigger = matches!(req.kind, OrderKind::Trigger { .. });
        if let Err(e) = self.validate_static(req, false) {
            return OrderStatusResp::Error(e);
        }
        if !is_trigger {
            if let Err(e) = self.validate_limit(user, req) {
                return OrderStatusResp::Error(e);
            }
        }
        let old_cloid = match found {
            Found::Book(oid, c) => {
                self.cancel_resting(req.asset, oid, OrderLifecycle::Canceled);
                c
            }
            Found::Trig(oid, c) => {
                self.cancel_trigger(req.asset, oid, OrderLifecycle::Canceled);
                c
            }
        };
        let mut req = req.clone();
        if req.cloid.is_none() {
            req.cloid = old_cloid;
        }
        // The replacement gets a new oid (HL semantics: modify = cancel + new order).
        self.place_request(user, &req, None, false)
    }

    // -------------------------------------------------------- validator txs

    fn exec_validator_tx(&mut self, v: &ValidatorTx, signer: Option<Address>) {
        let Some(signer) = signer else { return };
        if signer != v.validator {
            return;
        }
        let stake = self.genesis.validators.stake_of(&signer);
        if stake == 0 {
            return;
        }
        let last = self.core.validator_nonces.get(&signer).copied();
        if last.is_some_and(|l| v.nonce <= l) {
            return;
        }
        self.core.validator_nonces.insert(signer, v.nonce);
        let now = self.core.time;
        match &v.action {
            ValidatorAction::OraclePrices { prices } => {
                let valid: Vec<(AssetId, Px)> =
                    prices.iter().copied().filter(|(a, p)| (*a as usize) < self.books.len() && p.0 > 0).collect();
                for (a, p) in &valid {
                    self.core.oracle_subs.insert((*a, signer), (*p, now));
                }
                self.oracle.submit(signer, stake, &valid, now);
            }
            ValidatorAction::BridgeDeposit(att) => self.on_deposit_attestation(signer, att),
            ValidatorAction::WithdrawalSignature { withdrawal_id, signature } => {
                let params = self.genesis.bridge.clone();
                let hooks = self.cfg.bridge.clone();
                if let Some(w) = self.core.bridge.pending_withdrawals.get_mut(withdrawal_id) {
                    if w.signatures.iter().any(|(a, _)| *a == signer) {
                        return;
                    }
                    let digest = (hooks.withdrawal_digest)(w, &params);
                    if (hooks.recover)(&digest, signature) != Some(signer) {
                        return;
                    }
                    w.signatures.push((signer, *signature));
                    w.signatures.sort_by_key(|(a, _)| *a);
                    self.bridge_dirty = true;
                }
            }
            ValidatorAction::WithdrawalFinalized { withdrawal_id } => {
                if !self.core.bridge.pending_withdrawals.contains_key(withdrawal_id) {
                    return;
                }
                let votes = self.core.bridge.finalize_votes.entry(*withdrawal_id).or_default();
                votes.insert(signer);
                let st: u64 = votes.iter().map(|a| self.genesis.validators.stake_of(a)).sum();
                if st >= self.genesis.validators.quorum_stake() {
                    self.core.bridge.finalize_votes.remove(withdrawal_id);
                    self.core.bridge.pending_withdrawals.remove(withdrawal_id);
                }
                self.bridge_dirty = true;
            }
        }
    }

    fn on_deposit_attestation(&mut self, signer: Address, att: &DepositAttestation) {
        let b = &mut self.core.bridge;
        if b.processed_deposits.contains(&att.deposit_id) || att.amount.0 <= 0 {
            return;
        }
        let per_id = b.attestations.entry(att.deposit_id).or_default();
        // One attestation per validator per deposit id (first one counts).
        if per_id.values().any(|s| s.contains(&signer)) {
            return;
        }
        let set = per_id.entry(att.clone()).or_default();
        set.insert(signer);
        let st: u64 = set.iter().map(|a| self.genesis.validators.stake_of(a)).sum();
        self.bridge_dirty = true;
        if st >= self.genesis.validators.quorum_stake() {
            b.attestations.remove(&att.deposit_id);
            b.processed_deposits.insert(att.deposit_id);
            if self.ch.deposit(att.user, att.amount).is_ok() {
                self.core.flows.deposits += att.amount;
                let (time, tx_hash) = (self.core.time, Hash32::ZERO);
                self.events.push(Event::Ledger {
                    user: att.user,
                    time,
                    tx_hash,
                    delta: LedgerDelta::Deposit { usdc: att.amount },
                });
            }
        }
    }

    // ----------------------------------------------------------- end block

    fn update_prices(&mut self, now: TimestampMs) {
        let quorum = self.genesis.validators.quorum_stake();
        for a in 0..self.books.len() {
            if let Some(px) = self.oracle.aggregate(a as AssetId, now, quorum) {
                self.core.oracle_px[a] = px;
            }
            let oracle = self.core.oracle_px[a];
            let (bid, ask) = (self.books[a].best_bid(), self.books[a].best_ask());
            let (mark, mid) = match (bid, ask) {
                (Some(b), Some(k)) => (median3(oracle, b, k), Some(Px((b.0 + k.0) / 2))),
                _ => (oracle, None),
            };
            self.core.mark_px[a] = mark;
            self.ch.set_prices(a as AssetId, oracle, mark, mid, now);
        }
    }

    fn activate_triggers(&mut self, now: TimestampMs) {
        for a in 0..self.trigs.len() {
            let mark = self.core.mark_px[a];
            let fired = self.trigs[a].take_triggered(mark);
            if fired.is_empty() {
                continue;
            }
            self.touch_book(a as AssetId);
            for t in fired {
                self.events.push(Event::OrderUpdate { user: t.user, order: trigger_view(&t), status: OrderLifecycle::Triggered, time: now });
                let mut sz = t.sz;
                if t.reduce_only || t.is_position_tpsl {
                    let cap = self.ch.reduce_only_capacity(&t.user, t.asset, t.side);
                    sz = if t.is_position_tpsl { cap } else { sz.min(cap) };
                    if sz.0 <= 0 {
                        self.events.push(Event::OrderUpdate {
                            user: t.user,
                            order: trigger_view(&t),
                            status: OrderLifecycle::ReduceOnlyCanceled,
                            time: now,
                        });
                        continue;
                    }
                } else {
                    let mc = MarginCheck { user: t.user, asset: t.asset, side: t.side, px: t.limit_px, sz, reduce_only: false };
                    if self.ch.check_order_margin(&mc).is_err() {
                        self.events.push(Event::OrderUpdate {
                            user: t.user,
                            order: trigger_view(&t),
                            status: OrderLifecycle::MarginCanceled,
                            time: now,
                        });
                        continue;
                    }
                }
                let o = RestingOrder {
                    oid: t.oid,
                    user: t.user,
                    asset: t.asset,
                    side: t.side,
                    limit_px: t.limit_px,
                    sz,
                    orig_sz: sz,
                    reduce_only: t.reduce_only || t.is_position_tpsl,
                    cloid: t.cloid,
                    timestamp: now,
                    tif: if t.is_market { Tif::Ioc } else { Tif::Gtc },
                };
                let p = self.place_resting(o.clone(), None);
                self.taker_status(&o, &p);
            }
        }
    }

    fn run_liquidations(&mut self, now: TimestampMs) {
        let targets = self.ch.liquidation_targets();
        if targets.is_empty() {
            return;
        }
        let slip = self.genesis.params.liquidation_slippage;
        for t in targets {
            self.cancel_all_of(t.user, t.isolated_asset, OrderLifecycle::LiquidatedCanceled);
            let v = self.ch.account_view(&t.user);
            self.ledger(
                t.user,
                LedgerDelta::Liquidation {
                    account_value: v.margin_summary.account_value,
                    liquidated_ntl_pos: v.margin_summary.total_ntl_pos,
                },
            );
            for (asset, sz, worst) in &t.closes {
                if sz.0 == 0 || (*asset as usize) >= self.books.len() {
                    continue;
                }
                let mark = self.core.mark_px[*asset as usize];
                let side = if sz.0 > 0 { Side::Bid } else { Side::Ask };
                let limit = match side {
                    Side::Bid => {
                        let bound = mark + px_mul_rate(mark, slip, Rounding::Floor);
                        if worst.0 > 0 { (*worst).min(bound) } else { bound }
                    }
                    Side::Ask => {
                        let bound = mark - px_mul_rate(mark, slip, Rounding::Floor);
                        (*worst).max(bound)
                    }
                };
                let oid = self.alloc_oid();
                let o = RestingOrder {
                    oid,
                    user: t.user,
                    asset: *asset,
                    side,
                    limit_px: limit,
                    sz: sz.abs(),
                    orig_sz: sz.abs(),
                    reduce_only: true,
                    cloid: None,
                    timestamp: now,
                    tif: Tif::Ioc,
                };
                let info = LiquidationInfo { liquidated_user: t.user, mark_px: mark, method: LiquidationMethod::Market };
                self.place_resting(o, Some(info));
            }
            let ev = self.ch.backstop_and_adl(t.user, now);
            for mut e in ev {
                if let Event::Fill(f) = &mut e {
                    f.tid = self.core.next_tid;
                    self.core.next_tid += 1;
                }
                self.events.push(e);
            }
        }
    }

    // ---------------------------------------------------------- commitment

    fn hash_book(&self, a: usize) -> Hash32 {
        let mut h = blake3::Hasher::new();
        let b = self.books[a].encode();
        let t = self.trigs[a].encode();
        h.update(&(b.len() as u64).to_le_bytes());
        h.update(&b);
        h.update(&t);
        Hash32(*h.finalize().as_bytes())
    }

    /// Re-hash dirty entities and compute the root for `core.height`.
    fn commit(&mut self) -> Hash32 {
        for (k, h) in E::take_dirty(&mut self.ch) {
            self.ch_hashes.insert(k, h);
        }
        let c = &self.core;
        let g = GlobalView {
            time: c.time,
            next_oid: c.next_oid,
            next_tid: c.next_tid,
            flows: &c.flows,
            validator_nonces: &c.validator_nonces,
            tpsl_parents: &c.tpsl_parents,
        };
        self.own_hashes.insert(StateKey::Global, hash_borsh(&g));
        let o = OracleView { subs: &c.oracle_subs, oracle_px: &c.oracle_px, mark_px: &c.mark_px };
        self.own_hashes.insert(StateKey::Oracle, hash_borsh(&o));
        if self.bridge_dirty {
            self.own_hashes.insert(StateKey::Bridge, hash_borsh(&self.core.bridge));
            self.bridge_dirty = false;
        }
        for a in std::mem::take(&mut self.dirty_users) {
            match self.core.users.get(&a) {
                Some(m) if !m.is_empty() => {
                    self.own_hashes.insert(StateKey::UserMeta(a), hash_borsh(m));
                }
                _ => {
                    self.core.users.remove(&a);
                    self.own_hashes.remove(&StateKey::UserMeta(a));
                }
            }
        }
        for a in 0..self.dirty_books.len() {
            if self.dirty_books[a] {
                let h = self.hash_book(a);
                self.own_hashes.insert(StateKey::Book(a as AssetId), h);
                self.dirty_books[a] = false;
            }
        }
        self.compute_root()
    }

    fn compute_root(&self) -> Hash32 {
        let mut h = blake3::Hasher::new();
        h.update(&self.core.height.to_le_bytes());
        let mut buf = Vec::with_capacity(64);
        let mut feed = |k: &StateKey, v: &Hash32| {
            buf.clear();
            k.serialize(&mut buf).expect("vec write");
            buf.extend_from_slice(&v.0);
            h.update(&buf);
        };
        // Merge the two sorted maps (own entries first on equal keys).
        let mut a = self.own_hashes.iter().peekable();
        let mut b = self.ch_hashes.iter().peekable();
        loop {
            match (a.peek(), b.peek()) {
                (Some((ka, _)), Some((kb, _))) => {
                    if ka <= kb {
                        let (k, v) = a.next().expect("peeked");
                        feed(k, v);
                    } else {
                        let (k, v) = b.next().expect("peeked");
                        feed(k, v);
                    }
                }
                (Some(_), None) => {
                    let (k, v) = a.next().expect("peeked");
                    feed(k, v);
                }
                (None, Some(_)) => {
                    let (k, v) = b.next().expect("peeked");
                    feed(k, v);
                }
                (None, None) => break,
            }
        }
        Hash32(*h.finalize().as_bytes())
    }

    // ---------------------------------------------------- snapshot/restore

    pub fn make_snapshot(&self) -> Snapshot {
        let data = SnapshotData {
            core: self.core.clone(),
            ch: self.ch.encode(),
            books: self.books.iter().map(|b| b.encode()).collect(),
            trigs: self.trigs.iter().map(|t| t.encode()).collect(),
        };
        let raw = borsh::to_vec(&data).expect("encode");
        let data = zstd::encode_all(raw.as_slice(), 3).expect("zstd");
        Snapshot { height: self.core.height, state_root: self.current_root(), data }
    }

    pub fn restore_snapshot(&mut self, s: &Snapshot) -> Result<Hash32, String> {
        let raw = zstd::decode_all(s.data.as_slice()).map_err(|e| format!("zstd: {e}"))?;
        let d = SnapshotData::try_from_slice(&raw).map_err(|e| format!("decode: {e}"))?;
        let n = self.genesis.markets.len();
        if d.books.len() != n || d.trigs.len() != n || d.core.oracle_px.len() != n {
            return Err("snapshot market count mismatch".into());
        }
        let mut ch = E::decode_clearinghouse(&self.genesis, &d.ch)?;
        let ch_entities = E::all_entity_hashes(&mut ch);
        let mut books = Vec::with_capacity(n);
        let mut trigs = Vec::with_capacity(n);
        for (i, spec) in self.genesis.markets.iter().enumerate() {
            books.push(E::decode_book(spec, &d.books[i])?);
            trigs.push(E::decode_triggers(spec, &d.trigs[i])?);
        }
        let mut oracle = E::new_oracle(&self.genesis);
        for ((asset, v), (px, t)) in &d.core.oracle_subs {
            let stake = self.genesis.validators.stake_of(v);
            oracle.submit(*v, stake, &[(*asset, *px)], *t);
        }
        let mut core = d.core;
        core.rebuild_derived();
        self.dirty_users = core.users.keys().copied().collect();
        self.core = core;
        self.ch = ch;
        self.books = books;
        self.trigs = trigs;
        self.oracle = oracle;
        self.ch_hashes = ch_entities.into_iter().collect();
        self.own_hashes.clear();
        self.dirty_books = vec![true; n];
        self.bridge_dirty = true;
        self.events.clear();
        let root = self.commit();
        if root != s.state_root {
            return Err(format!("snapshot root mismatch: computed {root}, snapshot claims {}", s.state_root));
        }
        self.roots.clear();
        self.roots.push_back((self.core.height, root));
        Ok(root)
    }

    // ------------------------------------------------------------ check_tx

    pub fn admit(&self, tx: &Tx) -> Result<Address, AdmissionError> {
        match tx {
            Tx::User(u) => {
                let h = tx.hash();
                let r = self.recover_cached(&h, u);
                let (action, signer) = match &*r {
                    Ok(x) => x,
                    Err(e) if e == err_msg::INVALID_SIGNATURE => return Err(AdmissionError::BadSignature),
                    Err(e) => return Err(AdmissionError::Malformed(e.clone())),
                };
                if u.vault_address.is_some() {
                    return Err(AdmissionError::Malformed(err_msg::VAULT_UNSUPPORTED.into()));
                }
                if let Some(n) = embedded_nonce(action) {
                    if n != u.nonce {
                        return Err(AdmissionError::BadNonce(err_msg::NONCE_MISMATCH.into()));
                    }
                }
                let now = self.core.time;
                let meta = self.core.users.get(signer);
                let empty = UserMeta::default();
                let m = meta.unwrap_or(&empty);
                match m.check_nonce(u.nonce, now) {
                    Ok(()) => Ok(*signer),
                    // Before the first block the chain time is genesis time,
                    // which may be far in the past: skip the future bound.
                    Err(e) if self.core.height == 0 && u.nonce >= now && !m.nonces.contains(&u.nonce) => {
                        let _ = e;
                        Ok(*signer)
                    }
                    Err(e) => Err(AdmissionError::BadNonce(e)),
                }
            }
            Tx::Validator(v) => {
                let signer = self
                    .verifier
                    .recover_validator(v, self.genesis.chain_id)
                    .map_err(|_| AdmissionError::BadSignature)?;
                if signer != v.validator {
                    return Err(AdmissionError::BadSignature);
                }
                if !self.genesis.validators.contains(&signer) {
                    return Err(AdmissionError::UnknownSigner(signer));
                }
                if self.core.validator_nonces.get(&signer).is_some_and(|l| v.nonce <= *l) {
                    return Err(AdmissionError::BadNonce("validator nonce not increasing".into()));
                }
                Ok(signer)
            }
        }
    }
}

enum Pre {
    User(TxHash, Arc<Recovered>),
    Validator(Option<Address>),
}

impl<E: Engines, V: TxVerifier + 'static> Application for Chain<E, V> {
    fn genesis(&self) -> &Genesis {
        &self.genesis
    }
    fn executed_height(&self) -> Height {
        self.core.height
    }
    fn root_at(&self, h: Height) -> Option<Hash32> {
        let first = self.roots.front()?.0;
        if h < first {
            return None;
        }
        self.roots.get((h - first) as usize).filter(|r| r.0 == h).map(|r| r.1)
    }
    fn execute_block(&mut self, ctx: &BlockContext, txs: &[Tx]) -> BlockOutcome {
        self.execute(ctx, txs)
    }
    fn check_tx(&self, tx: &Tx) -> Result<Address, AdmissionError> {
        self.admit(tx)
    }
    fn snapshot(&self) -> Snapshot {
        self.make_snapshot()
    }
    fn restore(&mut self, s: &Snapshot) -> Result<Hash32, String> {
        self.restore_snapshot(s)
    }
    fn query(&self) -> &dyn StateQuery {
        self
    }
}
