//! Off-chain price feeds used by validators to produce oracle submissions.
//! These run inside the node, NOT in the state machine, but are still
//! integer-only (no floating point) and deterministic given their inputs.
//!
//! * [`SyntheticFeed`]: seeded random walk with jumps and mild mean
//!   reversion, evaluated on wall-time buckets so every validator with the
//!   same `common_seed` sees the same reference price, plus per-validator
//!   noise and an optional adversarial [`Skew`].
//! * [`FixedFeed`]: constant prices, shareable/updatable from tests.
//! * [`FilePriceFeed`]: re-reads `<asset> <px>` lines from a file.

use interfaces::*;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// ------------------------------------------------------------- hashing ---

/// SplitMix64 finalizer.
pub fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Counter-based random word for (seed, a, b, c): a pure function.
pub fn rand_word(seed: u64, a: u64, b: u64, c: u64) -> u64 {
    let mut h = splitmix64(seed ^ 0x6a09_e667_f3bc_c908);
    h = splitmix64(h ^ a);
    h = splitmix64(h ^ b.rotate_left(17));
    splitmix64(h ^ c.rotate_left(41))
}

/// Uniform integer in [-scale, scale] from a random word.
fn signed_unit(w: u64, scale: i128) -> i128 {
    let span = 2 * scale + 1;
    (w as i128).rem_euclid(span) - scale
}

// ------------------------------------------------------------- rounding ---

fn digits(mut v: u64) -> u32 {
    let mut n = 0;
    while v > 0 {
        v /= 10;
        n += 1;
    }
    n
}

/// Round `px` down to a valid HL price for `spec` (<= 5 significant figures
/// unless integer, <= `6 - sz_decimals` decimals). Never returns zero: if the
/// rounded value would be 0, the smallest valid tick is returned.
pub fn round_down_to_valid_px(px: Px, spec: &MarketSpec) -> Px {
    let n = digits(px.0);
    let one = PX_SCALE as u64; // 1.0 in Px units
    let unit_sig = if n > 5 { 10u64.pow(n - 5).min(one) } else { 1 };
    let unit_dec = 10u64.pow(PX_DECIMALS - spec.max_px_decimals().min(PX_DECIMALS));
    let unit = unit_sig.max(unit_dec);
    let r = px.0 / unit * unit;
    Px(if r == 0 { unit } else { r })
}

// ------------------------------------------------------------ synthetic ---

/// Adversarial bias applied by a validator to its own submissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Skew {
    #[default]
    None,
    /// Constant relative bias (e.g. +5% = `Rate::from_raw(50_000_000_000)`).
    Fixed(Rate),
    /// Bias growing linearly from `start_ms` by `per_sec` per second, with
    /// |bias| capped at `max`.
    Growing { per_sec: Rate, max: Rate, start_ms: u64 },
}

impl Skew {
    /// Relative bias at `now_ms`.
    pub fn bias(&self, now_ms: u64) -> Rate {
        match *self {
            Skew::None => Rate::ZERO,
            Skew::Fixed(r) => r,
            Skew::Growing { per_sec, max, start_ms } => {
                let secs = (now_ms.saturating_sub(start_ms) / 1000) as i128;
                let b = (per_sec.0 as i128).saturating_mul(secs);
                let cap = (max.0 as i128).abs();
                Rate::from_raw(b.clamp(-cap, cap))
            }
        }
    }
}

/// Parameters of the synthetic reference process (all integer).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SynthParams {
    /// Wall-time bucket length; the reference price is constant within one.
    pub bucket_ms: u64,
    /// Std-dev of the per-bucket relative return (Rate, 1e-12).
    pub vol_per_bucket: Rate,
    /// Probability of a jump per bucket, parts per million.
    pub jump_prob_ppm: u64,
    /// Maximum relative jump size; jumps are uniform in [size/2, size] with a
    /// random sign.
    pub jump_size: Rate,
    /// Mean reversion toward the initial price per bucket (Rate fraction of
    /// the relative deviation removed each bucket).
    pub mean_reversion: Rate,
    /// Amplitude of per-validator uniform noise (relative), redrawn every
    /// bucket.
    pub noise: Rate,
    /// Round outputs down to valid HL prices (`MarketSpec::is_valid_px`).
    pub round_to_valid_px: bool,
}

impl Default for SynthParams {
    fn default() -> Self {
        SynthParams {
            bucket_ms: 1000,
            // ~80% annualised vol at 1 s buckets: 0.8 / sqrt(31.5e6) ~ 1.4e-4
            vol_per_bucket: Rate::from_raw(140_000_000),
            // ~ one jump per hour
            jump_prob_ppm: 278,
            jump_size: Rate::from_raw(10_000_000_000), // 1%
            mean_reversion: Rate::from_raw(2_000_000), // 2e-6 / bucket
            noise: Rate::from_raw(100_000_000),        // 0.01%
            round_to_valid_px: true,
        }
    }
}

/// Sum of 4 uniforms on [0, 2^16): mean 2^17, std-dev 2^16/sqrt(3) ~ 37837.
const IH_SD: i128 = 37_837;

#[derive(Clone, Debug)]
struct WalkState {
    spec: MarketSpec,
    /// Last computed bucket and its reference price (raw Px units).
    bucket: u64,
    px: i128,
}

/// Deterministic synthetic price feed. See module docs.
#[derive(Clone, Debug)]
pub struct SyntheticFeed {
    params: SynthParams,
    start_ms: u64,
    common_seed: u64,
    validator_seed: u64,
    skew: Skew,
    walks: Vec<WalkState>,
}

impl SyntheticFeed {
    /// `start_ms` (normally genesis time) anchors bucket 0, where every
    /// market is at `MarketSpec::initial_px`. All validators sharing
    /// `common_seed`, `start_ms`, `params` and `markets` compute identical
    /// reference prices; `validator_seed` only drives the noise.
    pub fn new(markets: &[MarketSpec], params: SynthParams, start_ms: u64, common_seed: u64, validator_seed: u64) -> Self {
        assert!(params.bucket_ms > 0, "bucket_ms must be > 0");
        let walks = markets
            .iter()
            .map(|m| WalkState { spec: m.clone(), bucket: 0, px: m.initial_px.0 as i128 })
            .collect();
        SyntheticFeed { params, start_ms, common_seed, validator_seed, skew: Skew::None, walks }
    }

    pub fn with_skew(mut self, skew: Skew) -> Self {
        self.skew = skew;
        self
    }

    pub fn set_skew(&mut self, skew: Skew) {
        self.skew = skew;
    }

    pub fn bucket_of(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.start_ms) / self.params.bucket_ms
    }

    /// Relative return applied going from bucket `b - 1` to bucket `b`.
    fn step_return(&self, asset: AssetId, b: u64, px: i128, initial: i128) -> i128 {
        let p = &self.params;
        let a = asset as u64;
        let w = rand_word(self.common_seed, a, b, 0);
        let ih: i128 = (0..4).map(|k| ((w >> (16 * k)) & 0xffff) as i128).sum::<i128>() - (1 << 17);
        let mut r = p.vol_per_bucket.0 as i128 * ih / IH_SD;
        let wj = rand_word(self.common_seed, a, b, 1);
        if (wj % 1_000_000) < p.jump_prob_ppm {
            let size = p.jump_size.0 as i128;
            let half = size / 2;
            let mag = half + ((wj >> 20) as i128).rem_euclid(size - half + 1);
            r += if (wj >> 63) == 1 { -mag } else { mag };
        }
        // mean reversion: r -= mr * (px - initial) / initial
        r -= p.mean_reversion.0 as i128 * (px - initial) / initial.max(1);
        let lim = RATE_SCALE / 2;
        r.clamp(-lim, lim)
    }

    /// Reference price (before noise/skew/rounding) of market index `i` at bucket `b`.
    fn reference_at(&mut self, i: usize, b: u64) -> i128 {
        let (asset, initial) = (self.walks[i].spec.asset, self.walks[i].spec.initial_px.0 as i128);
        if b < self.walks[i].bucket {
            self.walks[i].bucket = 0;
            self.walks[i].px = initial;
        }
        let floor = (initial / 1000).max(1);
        let ceil = (u64::MAX / 4) as i128;
        let mut px = self.walks[i].px;
        for k in self.walks[i].bucket + 1..=b {
            let r = self.step_return(asset, k, px, initial);
            px = (px * (RATE_SCALE + r) / RATE_SCALE).clamp(floor, ceil);
        }
        self.walks[i].bucket = b;
        self.walks[i].px = px;
        px
    }

    /// The common (noise-free, unskewed, unrounded) reference price.
    pub fn reference_px(&mut self, asset: AssetId, now_ms: u64) -> Option<Px> {
        let b = self.bucket_of(now_ms);
        let i = self.walks.iter().position(|w| w.spec.asset == asset)?;
        Some(Px(self.reference_at(i, b) as u64))
    }
}

impl PriceFeed for SyntheticFeed {
    fn prices(&mut self, now_ms: u64) -> Vec<(AssetId, Px)> {
        let b = self.bucket_of(now_ms);
        let bias = self.skew.bias(now_ms).0 as i128;
        let mut out = Vec::with_capacity(self.walks.len());
        for i in 0..self.walks.len() {
            let r = self.reference_at(i, b);
            let asset = self.walks[i].spec.asset;
            let nw = rand_word(self.validator_seed, asset as u64, b, 2);
            let noise = signed_unit(nw, self.params.noise.0 as i128);
            let rel = (RATE_SCALE + noise + bias).max(1);
            let px = (r * rel / RATE_SCALE).clamp(1, u64::MAX as i128) as u64;
            let px = if self.params.round_to_valid_px { round_down_to_valid_px(Px(px), &self.walks[i].spec) } else { Px(px) };
            out.push((asset, px));
        }
        out.sort_by_key(|(a, _)| *a);
        out
    }
}

// ---------------------------------------------------------------- fixed ---

/// Constant prices. Clones share the same price table, so a test can keep a
/// clone and move prices while the node owns the feed.
#[derive(Clone, Debug, Default)]
pub struct FixedFeed {
    prices: Arc<Mutex<BTreeMap<AssetId, Px>>>,
}

impl FixedFeed {
    pub fn new(prices: &[(AssetId, Px)]) -> Self {
        FixedFeed { prices: Arc::new(Mutex::new(prices.iter().copied().collect())) }
    }

    /// From each market's `initial_px`.
    pub fn from_markets(markets: &[MarketSpec]) -> Self {
        let v: Vec<_> = markets.iter().map(|m| (m.asset, m.initial_px)).collect();
        Self::new(&v)
    }

    pub fn set(&self, asset: AssetId, px: Px) {
        self.prices.lock().expect("fixed feed lock").insert(asset, px);
    }

    pub fn remove(&self, asset: AssetId) {
        self.prices.lock().expect("fixed feed lock").remove(&asset);
    }
}

impl PriceFeed for FixedFeed {
    fn prices(&mut self, _now_ms: u64) -> Vec<(AssetId, Px)> {
        self.prices.lock().expect("fixed feed lock").iter().map(|(a, p)| (*a, *p)).collect()
    }
}

// ----------------------------------------------------------------- file ---

/// Reads prices from a text file on every call: one `<asset_id> <px>` per
/// line (`px` decimal, e.g. `0 65000.5`), `#` comments and blank lines
/// ignored. If the file is missing or malformed, the last good prices are
/// returned.
#[derive(Clone, Debug)]
pub struct FilePriceFeed {
    path: PathBuf,
    last: Vec<(AssetId, Px)>,
}

impl FilePriceFeed {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        FilePriceFeed { path: path.into(), last: Vec::new() }
    }

    /// Parse the file format. Later lines for the same asset win.
    pub fn parse(text: &str) -> Result<Vec<(AssetId, Px)>, String> {
        let mut m = BTreeMap::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.split_whitespace();
            let (a, p) = match (it.next(), it.next(), it.next()) {
                (Some(a), Some(p), None) => (a, p),
                _ => return Err(format!("line {}: expected `<asset> <px>`", n + 1)),
            };
            let a: AssetId = a.parse().map_err(|_| format!("line {}: bad asset id", n + 1))?;
            let p = Px::parse(p).map_err(|e| format!("line {}: bad px: {e:?}", n + 1))?;
            if p.0 == 0 {
                return Err(format!("line {}: zero px", n + 1));
            }
            m.insert(a, p);
        }
        Ok(m.into_iter().collect())
    }
}

impl PriceFeed for FilePriceFeed {
    fn prices(&mut self, _now_ms: u64) -> Vec<(AssetId, Px)> {
        if let Ok(text) = std::fs::read_to_string(&self.path) {
            if let Ok(v) = Self::parse(&text) {
                self.last = v;
            }
        }
        self.last.clone()
    }
}
