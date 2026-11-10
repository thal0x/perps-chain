//! In-memory fake [`ApiBackend`] with a mutable fake [`StateQuery`] and a
//! scripted block stream. Used by this crate's tests; usable by others.

use interfaces::*;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

#[derive(Clone)]
pub struct FakeState {
    pub markets: Vec<MarketSpec>,
    pub ctxs: HashMap<AssetId, MarketCtx>,
    pub books: HashMap<AssetId, (Vec<(Px, Sz, u32)>, Vec<(Px, Sz, u32)>)>,
    pub accounts: HashMap<Address, AccountView>,
    pub orders: HashMap<Address, Vec<RestingOrderView>>,
    pub agents: HashMap<Address, Address>,
    pub time: TimestampMs,
    pub height: Height,
    pub withdrawals: Vec<PendingWithdrawal>,
    pub validators: ValidatorSet,
}

impl Default for FakeState {
    fn default() -> Self {
        Self {
            markets: vec![],
            ctxs: HashMap::new(),
            books: HashMap::new(),
            accounts: HashMap::new(),
            orders: HashMap::new(),
            agents: HashMap::new(),
            time: 0,
            height: 0,
            withdrawals: vec![],
            validators: ValidatorSet { validators: vec![] },
        }
    }
}

impl StateQuery for FakeState {
    fn markets(&self) -> Vec<MarketSpec> {
        self.markets.clone()
    }
    fn market_ctx(&self, asset: AssetId) -> MarketCtx {
        self.ctxs.get(&asset).cloned().unwrap_or(MarketCtx {
            asset,
            oracle_px: Px::ZERO,
            mark_px: Px::ZERO,
            mid_px: None,
            funding: Rate::ZERO,
            premium: Rate::ZERO,
            open_interest: Sz::ZERO,
            day_ntl_vlm: Usd::ZERO,
            prev_day_px: Px::ZERO,
        })
    }
    fn mid(&self, asset: AssetId) -> Option<Px> {
        let (b, a) = self.books.get(&asset)?;
        let (b, a) = (b.first()?.0, a.first()?.0);
        Some(Px((b.0 + a.0) / 2))
    }
    fn l2_book(&self, asset: AssetId, depth: usize) -> (Vec<(Px, Sz, u32)>, Vec<(Px, Sz, u32)>) {
        match self.books.get(&asset) {
            Some((b, a)) => (b.iter().take(depth).copied().collect(), a.iter().take(depth).copied().collect()),
            None => (vec![], vec![]),
        }
    }
    fn account(&self, user: &Address) -> AccountView {
        self.accounts.get(user).cloned().unwrap_or_default()
    }
    fn open_orders(&self, user: &Address) -> Vec<RestingOrderView> {
        self.orders.get(user).cloned().unwrap_or_default()
    }
    fn order_by_oid(&self, user: &Address, oid: Oid) -> Option<RestingOrderView> {
        self.orders.get(user)?.iter().find(|o| o.oid == oid).cloned()
    }
    fn order_by_cloid(&self, user: &Address, cloid: &Cloid) -> Option<RestingOrderView> {
        self.orders.get(user)?.iter().find(|o| o.cloid.as_ref() == Some(cloid)).cloned()
    }
    fn principal_of(&self, signer: &Address) -> Address {
        self.agents.get(signer).copied().unwrap_or(*signer)
    }
    fn block_time(&self) -> TimestampMs {
        self.time
    }
    fn height(&self) -> Height {
        self.height
    }
    fn pending_withdrawals(&self) -> Vec<PendingWithdrawal> {
        self.withdrawals.clone()
    }
    fn processed_deposit(&self, _deposit_id: u64) -> bool {
        false
    }
    fn validator_set(&self) -> ValidatorSet {
        self.validators.clone()
    }
}

/// Scripted outcome of one submitted tx.
#[derive(Clone, Debug)]
pub enum Script {
    /// Include the tx in a block with this receipt result and events.
    Execute { user: Address, result: Result<ActionResponse, String>, events: Vec<Event> },
    /// Reject at admission.
    Reject(AdmissionError),
    /// Accept but never include (receipt never arrives).
    Drop,
}

pub struct FakeBackend {
    pub genesis: Genesis,
    pub state: Mutex<FakeState>,
    callbacks: Mutex<Vec<BlockCallback>>,
    script: Mutex<VecDeque<Script>>,
    pub submitted: Mutex<Vec<(Tx, TxSource)>>,
    pub chaos: Mutex<Option<String>>,
    height: Mutex<Height>,
    /// If set, user txs whose signature does not recover (hlwire, testnet
    /// flavour) are rejected with `BadSignature`; recovered signers are logged.
    pub verify_signatures: std::sync::atomic::AtomicBool,
    pub signers: Mutex<Vec<Address>>,
    /// Used when the script queue is empty.
    responder: Mutex<Option<Box<dyn Fn(&UserTx) -> Script + Send + Sync>>>,
}

pub fn addr(n: u8) -> Address {
    let mut a = [0u8; 20];
    a[19] = n;
    a[0] = 0xab;
    Address(a)
}

pub fn px(s: &str) -> Px {
    Px::parse(s).unwrap()
}
pub fn sz(s: &str) -> Sz {
    Sz::parse(s).unwrap()
}
pub fn usd(s: &str) -> Usd {
    Usd::parse(s).unwrap()
}

/// Test genesis: ETH (asset 0, szDecimals 4), BTC (asset 1, szDecimals 5);
/// spot USDC (0) and PURR (1).
pub fn test_genesis() -> Genesis {
    let m = |asset, name: &str, szd, lev, p: &str| MarketSpec {
        asset,
        name: name.into(),
        sz_decimals: szd,
        max_leverage: lev,
        only_isolated: false,
        initial_px: px(p),
    };
    let mut purr = [0u8; 16];
    purr.copy_from_slice(&hex::decode("c4bf3f870c0e9465323c0b6ed28096c2").unwrap());
    let mut usdc = [0u8; 16];
    usdc.copy_from_slice(&hex::decode("6d1e7cde53ba9467b783cb7c530ce054").unwrap());
    Genesis {
        chain_id: 1337,
        is_mainnet: false,
        genesis_time_ms: 1_700_000_000_000,
        validators: ValidatorSet { validators: vec![] },
        markets: vec![m(0, "ETH", 4, 50, "3000"), m(1, "BTC", 5, 40, "60000")],
        balances: vec![],
        spot_tokens: vec![
            SpotToken { index: 0, name: "USDC".into(), sz_decimals: 8, wei_decimals: 8, token_id: usdc },
            SpotToken { index: 1, name: "PURR".into(), sz_decimals: 0, wei_decimals: 5, token_id: purr },
        ],
        spot_balances: vec![],
        params: ChainParams::default(),
        bridge: BridgeParams { eth_chain_id: 31337, contract: Address::ZERO, min_deposit: Usd::ZERO },
    }
}

impl FakeBackend {
    pub fn new(genesis: Genesis) -> Arc<Self> {
        let st = FakeState { markets: genesis.markets.clone(), validators: genesis.validators.clone(), ..Default::default() };
        Arc::new(Self {
            genesis,
            state: Mutex::new(st),
            callbacks: Mutex::new(vec![]),
            script: Mutex::new(VecDeque::new()),
            submitted: Mutex::new(vec![]),
            chaos: Mutex::new(None),
            height: Mutex::new(0),
            responder: Mutex::new(None),
            verify_signatures: std::sync::atomic::AtomicBool::new(false),
            signers: Mutex::new(vec![]),
        })
    }

    pub fn set_responder(&self, f: impl Fn(&UserTx) -> Script + Send + Sync + 'static) {
        *self.responder.lock() = Some(Box::new(f));
    }

    pub fn script(&self, s: Script) {
        self.script.lock().push_back(s);
    }

    /// Emit an executed block to all registered callbacks.
    pub fn push_block(&self, receipts: Vec<TxReceipt>, events: Vec<Event>) -> Height {
        let height = {
            let mut h = self.height.lock();
            *h += 1;
            *h
        };
        self.state.lock().height = height;
        let b = Arc::new(BlockOutcome { height, state_root: Hash32::ZERO, receipts, events, order_actions: 0 });
        for cb in self.callbacks.lock().iter() {
            cb(b.clone());
        }
        height
    }
}

impl ApiBackend for FakeBackend {
    fn genesis(&self) -> Genesis {
        self.genesis.clone()
    }
    fn query(&self, f: &mut dyn FnMut(&dyn StateQuery)) {
        let st = self.state.lock();
        f(&*st);
    }
    fn submit(&self, tx: Tx, source: TxSource) -> Result<TxHash, AdmissionError> {
        let h = tx.hash();
        if let Tx::User(u) = &tx {
            if self.verify_signatures.load(std::sync::atomic::Ordering::Relaxed) {
                match hlwire::recover_signer(u, self.genesis.is_mainnet) {
                    Ok(a) => self.signers.lock().push(a),
                    Err(_) => return Err(AdmissionError::BadSignature),
                }
            }
        }
        let scripted = self.script.lock().pop_front();
        let s = match scripted {
            Some(s) => s,
            None => match (&*self.responder.lock(), &tx) {
                (Some(r), Tx::User(u)) => r(u),
                _ => Script::Execute { user: Address::ZERO, result: Ok(ActionResponse::Default), events: vec![] },
            },
        };
        self.submitted.lock().push((tx, source));
        match s {
            Script::Reject(e) => Err(e),
            Script::Drop => Ok(h),
            Script::Execute { user, result, events } => {
                self.push_block(vec![TxReceipt { tx_hash: h, user, result }], events);
                Ok(h)
            }
        }
    }
    fn on_block(&self, cb: BlockCallback) {
        self.callbacks.lock().push(cb);
    }
    fn status(&self) -> NodeStatus {
        let h = *self.height.lock();
        NodeStatus { committed_height: h, executed_height: h, mempool_len: 0, ..Default::default() }
    }
    fn set_chaos(&self, cfg_json: &str) -> Result<(), String> {
        serde_json::from_str::<serde_json::Value>(cfg_json).map_err(|e| e.to_string())?;
        *self.chaos.lock() = Some(cfg_json.to_string());
        Ok(())
    }
}

/// HL action `type` of a msgpack-encoded action (first map entry "type").
pub fn action_type(action_bytes: &[u8]) -> Option<String> {
    // fixmap header, then fixstr "type", then fixstr/str8 value
    let rest = action_bytes.get(1..)?;
    let rest = rest.strip_prefix(b"\xa4type")?;
    let (len, body) = match *rest.first()? {
        b @ 0xa0..=0xbf => ((b & 0x1f) as usize, rest.get(1..)?),
        0xd9 => (*rest.get(1)? as usize, rest.get(2..)?),
        _ => return None,
    };
    String::from_utf8(body.get(..len)?.to_vec()).ok()
}

/// Plausible receipts by action type: every order rests (oid counter, one
/// status per `"b"` key), cancels succeed, everything else is `default`.
pub fn simple_responder(user: Address) -> impl Fn(&UserTx) -> Script + Send + Sync + 'static {
    let next = std::sync::atomic::AtomicU64::new(1000);
    move |u: &UserTx| {
        let ty = action_type(&u.action_bytes).unwrap_or_default();
        let count = |pat: &[u8]| u.action_bytes.windows(pat.len()).filter(|w| *w == pat).count().max(1);
        let result = match ty.as_str() {
            "order" | "batchModify" => {
                let n = count(b"\xa1b");
                Ok(ActionResponse::Order(
                    (0..n)
                        .map(|_| OrderStatusResp::Resting { oid: next.fetch_add(1, std::sync::atomic::Ordering::Relaxed), cloid: None })
                        .collect(),
                ))
            }
            "cancel" => Ok(ActionResponse::Cancel(vec![SimpleStatus::Success; count(b"\xa1o")])),
            "cancelByCloid" => Ok(ActionResponse::Cancel(vec![SimpleStatus::Success; count(b"\xa5cloid")])),
            _ => Ok(ActionResponse::Default),
        };
        Script::Execute { user, result, events: vec![] }
    }
}
