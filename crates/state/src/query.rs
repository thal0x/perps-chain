//! `StateQuery` implementation (read-only API surface).

use crate::chain::Chain;
use crate::engines::Engines;
use crate::views::*;
use interfaces::*;

impl<E: Engines, V: TxVerifier> StateQuery for Chain<E, V> {
    fn markets(&self) -> Vec<MarketSpec> {
        self.genesis.markets.clone()
    }
    fn market_ctx(&self, asset: AssetId) -> MarketCtx {
        self.ch.market_ctx(asset)
    }
    fn mid(&self, asset: AssetId) -> Option<Px> {
        let b = self.books.get(asset as usize)?;
        match (b.best_bid(), b.best_ask()) {
            (Some(x), Some(y)) => Some(Px((x.0 + y.0) / 2)),
            _ => None,
        }
    }
    fn l2_book(&self, asset: AssetId, depth: usize) -> (Vec<(Px, Sz, u32)>, Vec<(Px, Sz, u32)>) {
        match self.books.get(asset as usize) {
            Some(b) => (b.levels(Side::Bid, depth), b.levels(Side::Ask, depth)),
            None => (vec![], vec![]),
        }
    }
    fn account(&self, user: &Address) -> AccountView {
        self.ch.account_view(user)
    }
    fn open_orders(&self, user: &Address) -> Vec<RestingOrderView> {
        let mut v = Vec::new();
        for (b, t) in self.books.iter().zip(&self.trigs) {
            v.extend(b.user_orders(user).into_iter().map(resting_view));
            v.extend(t.user_orders(user).into_iter().map(trigger_view));
        }
        v
    }
    fn order_by_oid(&self, user: &Address, oid: Oid) -> Option<RestingOrderView> {
        for (b, t) in self.books.iter().zip(&self.trigs) {
            if let Some(o) = b.get(oid).filter(|o| &o.user == user) {
                return Some(resting_view(o));
            }
            if let Some(o) = t.get(oid).filter(|o| &o.user == user) {
                return Some(trigger_view(o));
            }
        }
        None
    }
    fn order_by_cloid(&self, user: &Address, cloid: &Cloid) -> Option<RestingOrderView> {
        for (b, t) in self.books.iter().zip(&self.trigs) {
            if let Some(o) = b.get_by_cloid(user, cloid) {
                return Some(resting_view(o));
            }
            if let Some(o) = t.user_orders(user).into_iter().find(|o| o.cloid.as_ref() == Some(cloid)) {
                return Some(trigger_view(o));
            }
        }
        None
    }
    fn principal_of(&self, signer: &Address) -> Address {
        *self.core.agents.get(signer).unwrap_or(signer)
    }
    fn leverage_of(&self, user: &Address, asset: AssetId) -> Option<Leverage> {
        if (asset as usize) < self.genesis.markets.len() {
            Some(self.ch.leverage_of(user, asset))
        } else {
            None
        }
    }
    /// Timestamp of the last executed block (genesis time before block 1).
    fn block_time(&self) -> TimestampMs {
        self.core.time
    }
    fn height(&self) -> Height {
        self.core.height
    }
    fn pending_withdrawals(&self) -> Vec<PendingWithdrawal> {
        self.core.bridge.pending_withdrawals.values().cloned().collect()
    }
    fn processed_deposit(&self, deposit_id: u64) -> bool {
        self.core.bridge.processed_deposits.contains(&deposit_id)
    }
    fn validator_set(&self) -> ValidatorSet {
        self.genesis.validators.clone()
    }
}
