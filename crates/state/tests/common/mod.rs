#![allow(dead_code)]
use interfaces::*;
pub use state::testkit::*;
use state::{ChainConfig, FakeChain};
use std::sync::Arc;

pub struct H {
    pub chain: FakeChain,
    pub time: u64,
    pub nonce: u64,
    pub outcomes: Vec<BlockOutcome>,
}

pub const MM: u64 = 1;
pub const ALICE: u64 = 2;
pub const BOB: u64 = 3;

impl H {
    pub fn new(balances: &[(Address, &str)]) -> Self {
        Self::with_validators(4, balances)
    }
    pub fn with_validators(n: usize, balances: &[(Address, &str)]) -> Self {
        let b: Vec<(Address, Usd)> = balances.iter().map(|(a, s)| (*a, usd(s))).collect();
        Self::from_genesis(test_genesis(n, &b))
    }
    pub fn from_genesis(g: Genesis) -> Self {
        let chain = FakeChain::new(g, Arc::new(FakeVerifier), ChainConfig::strict());
        Self { chain, time: GENESIS_TIME, nonce: GENESIS_TIME, outcomes: vec![] }
    }
    pub fn next_nonce(&mut self) -> u64 {
        self.nonce = self.nonce.max(self.time) + 1;
        self.nonce
    }
    pub fn ctx(&mut self, dt: u64) -> BlockContext {
        self.time += dt;
        BlockContext {
            height: self.chain.query().height() + 1,
            timestamp_ms: self.time,
            proposer: addr(1000),
            block_id: Hash32::ZERO,
        }
    }
    pub fn block(&mut self, txs: Vec<Tx>) -> BlockOutcome {
        let ctx = self.ctx(1000);
        let o = self.chain.execute_block(&ctx, &txs);
        assert!(self.chain.invariant_violations().is_empty());
        self.outcomes.push(o.clone());
        o
    }
    /// Single-tx block; returns the receipt result.
    pub fn send(&mut self, signer: Address, action: Action) -> Result<ActionResponse, String> {
        let n = self.next_nonce();
        let tx = user_tx(signer, &action, n);
        let o = self.block(vec![tx]);
        o.receipts[0].result.clone()
    }
    /// For user-signed actions whose embedded time must equal the nonce.
    pub fn send_with_nonce(&mut self, signer: Address, f: impl FnOnce(u64) -> Action) -> Result<ActionResponse, String> {
        let n = self.next_nonce();
        let tx = user_tx(signer, &f(n), n);
        let o = self.block(vec![tx]);
        o.receipts[0].result.clone()
    }
    pub fn order(&mut self, signer: Address, orders: Vec<OrderRequest>) -> Vec<OrderStatusResp> {
        self.order_grouped(signer, orders, Grouping::Na)
    }
    pub fn order_grouped(&mut self, signer: Address, orders: Vec<OrderRequest>, grouping: Grouping) -> Vec<OrderStatusResp> {
        match self.send(signer, Action::Order { orders, grouping }) {
            Ok(ActionResponse::Order(s)) => s,
            other => panic!("unexpected order response {other:?}"),
        }
    }
    pub fn order1(&mut self, signer: Address, o: OrderRequest) -> OrderStatusResp {
        self.order(signer, vec![o]).remove(0)
    }
    pub fn cancel(&mut self, signer: Address, asset: AssetId, oid: Oid) -> SimpleStatus {
        match self.send(signer, Action::Cancel { cancels: vec![(asset, oid)] }) {
            Ok(ActionResponse::Cancel(mut s)) => s.remove(0),
            other => panic!("unexpected cancel response {other:?}"),
        }
    }
    /// All validators submit the same oracle price for `asset`.
    pub fn oracle(&mut self, asset: AssetId, p: &str) {
        let vs: Vec<Address> = self.chain.query().validator_set().validators.iter().map(|v| v.address).collect();
        let txs = vs
            .iter()
            .map(|v| {
                self.nonce += 1;
                validator_tx(*v, self.nonce, ValidatorAction::OraclePrices { prices: vec![(asset, px(p))] })
            })
            .collect();
        self.block(txs);
    }
    pub fn pos(&self, user: Address, asset: AssetId) -> Sz {
        self.chain.clearinghouse().position(&user, asset)
    }
    pub fn last_events(&self) -> &[Event] {
        &self.outcomes.last().unwrap().events
    }
    pub fn all_events(&self) -> impl Iterator<Item = &Event> {
        self.outcomes.iter().flat_map(|o| o.events.iter())
    }
}

pub fn resting_oid(s: &OrderStatusResp) -> Oid {
    match s {
        OrderStatusResp::Resting { oid, .. } => *oid,
        other => panic!("expected resting, got {other:?}"),
    }
}

pub fn err_of(s: &OrderStatusResp) -> &str {
    match s {
        OrderStatusResp::Error(e) => e,
        other => panic!("expected error, got {other:?}"),
    }
}

use interfaces::Application;
pub trait QueryExt {
    fn q(&self) -> &dyn StateQuery;
}
impl QueryExt for FakeChain {
    fn q(&self) -> &dyn StateQuery {
        self.query()
    }
}
