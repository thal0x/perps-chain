//! Coin name <-> asset resolution (perps by name, spot pairs by "BASE/USDC"
//! or "@<index>").

use interfaces::*;
use std::collections::HashMap;

/// HL: spot asset id = 10000 + spot pair index.
pub const SPOT_ASSET_OFFSET: AssetId = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CoinRef {
    Perp(AssetId),
    /// Spot pair index (HL spot universe index; asset id = 10000 + index).
    Spot(u32),
}

#[derive(Clone, Debug)]
pub struct SpotPair {
    pub index: u32,
    pub name: String,
    pub base: u32,
    pub quote: u32,
    pub is_canonical: bool,
}

#[derive(Clone, Debug)]
pub struct Coins {
    pub markets: Vec<MarketSpec>,
    pub spot_tokens: Vec<SpotToken>,
    pub spot_pairs: Vec<SpotPair>,
    by_name: HashMap<String, CoinRef>,
}

impl Coins {
    pub fn new(genesis: &Genesis) -> Self {
        let mut markets = genesis.markets.clone();
        markets.sort_by_key(|m| m.asset);
        let mut spot_tokens = genesis.spot_tokens.clone();
        spot_tokens.sort_by_key(|t| t.index);
        // One canonical pair per non-USDC token: pair i = token i+1 / USDC.
        // The first pair is named "<BASE>/USDC" (HL: PURR/USDC is pair 0);
        // others are "@i". We guarantee "@1" exists (the SDK's basic_ws
        // example subscribes to it) by adding an alias pair if needed.
        let mut spot_pairs: Vec<SpotPair> = Vec::new();
        let quote = spot_tokens.first().map(|t| t.index).unwrap_or(0);
        for t in spot_tokens.iter().filter(|t| t.index != quote) {
            let index = spot_pairs.len() as u32;
            let name = if index == 0 { format!("{}/USDC", t.name) } else { format!("@{index}") };
            spot_pairs.push(SpotPair { index, name, base: t.index, quote, is_canonical: index == 0 });
        }
        if spot_pairs.len() == 1 {
            let base = spot_pairs[0].base;
            spot_pairs.push(SpotPair { index: 1, name: "@1".into(), base, quote, is_canonical: false });
        }
        let mut by_name = HashMap::new();
        for p in &spot_pairs {
            by_name.insert(p.name.clone(), CoinRef::Spot(p.index));
            by_name.insert(format!("@{}", p.index), CoinRef::Spot(p.index));
        }
        for m in &markets {
            by_name.insert(m.name.clone(), CoinRef::Perp(m.asset));
        }
        Coins { markets, spot_tokens, spot_pairs, by_name }
    }

    pub fn resolve(&self, coin: &str) -> Option<CoinRef> {
        self.by_name.get(coin).copied()
    }

    pub fn perp(&self, coin: &str) -> Option<AssetId> {
        match self.resolve(coin) {
            Some(CoinRef::Perp(a)) => Some(a),
            _ => None,
        }
    }

    pub fn market(&self, asset: AssetId) -> Option<&MarketSpec> {
        self.markets.get(asset as usize).filter(|m| m.asset == asset).or_else(|| self.markets.iter().find(|m| m.asset == asset))
    }

    /// HL coin name of an asset: perp name, or the spot pair name for
    /// `10000 + pair` ("PURR/USDC", "@1"); "?<id>" if unknown.
    pub fn name(&self, asset: AssetId) -> String {
        if let Some(p) = self.spot_pair_of_asset(asset) {
            return p.name.clone();
        }
        self.market(asset).map(|m| m.name.clone()).unwrap_or_else(|| format!("?{asset}"))
    }

    /// Asset id (10000 + index) of a coin, perp or spot.
    pub fn asset(&self, coin: &str) -> Option<AssetId> {
        match self.resolve(coin)? {
            CoinRef::Perp(a) => Some(a),
            CoinRef::Spot(i) => Some(SPOT_ASSET_OFFSET + i),
        }
    }

    pub fn spot_pair_of_asset(&self, asset: AssetId) -> Option<&SpotPair> {
        asset.checked_sub(SPOT_ASSET_OFFSET).and_then(|i| self.spot_pair(i))
    }

    pub fn is_spot_asset(&self, asset: AssetId) -> bool {
        self.spot_pair_of_asset(asset).is_some()
    }

    pub fn token(&self, index: u32) -> Option<&SpotToken> {
        self.spot_tokens.iter().find(|t| t.index == index)
    }

    pub fn token_by_name(&self, name: &str) -> Option<&SpotToken> {
        self.spot_tokens.iter().find(|t| t.name == name)
    }

    /// "NAME:0x<tokenId>" wire string for a token.
    pub fn token_wire(&self, index: u32) -> String {
        match self.token(index) {
            Some(t) => format!("{}:0x{}", t.name, hex::encode(t.token_id)),
            None => format!("?{index}"),
        }
    }

    pub fn spot_pair(&self, index: u32) -> Option<&SpotPair> {
        self.spot_pairs.iter().find(|p| p.index == index)
    }
}
