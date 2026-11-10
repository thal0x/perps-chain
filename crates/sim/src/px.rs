//! Integer price / size helpers producing HL-valid values
//! (`MarketSpec::is_valid_px` / `is_valid_sz`). No floating point.

use interfaces::*;
use rand::Rng;

fn digits(mut v: u64) -> u32 {
    let mut n = 0;
    while v > 0 {
        v /= 10;
        n += 1;
    }
    n
}

/// Smallest price increment (raw Px units) valid at magnitude `px` for `spec`:
/// max(5-significant-figure unit (capped at 1.0), decimal unit).
pub fn px_unit(spec: &MarketSpec, px: u64) -> u64 {
    let n = digits(px);
    let one = PX_SCALE as u64;
    let unit_sig = if n > 5 { 10u64.pow(n - 5).min(one) } else { 1 };
    let unit_dec = 10u64.pow(PX_DECIMALS - spec.max_px_decimals().min(PX_DECIMALS));
    unit_sig.max(unit_dec)
}

/// Round a raw price to a valid HL price (down or up). Never returns zero.
pub fn round_px(spec: &MarketSpec, px: u64, up: bool) -> Px {
    let px = px.max(1);
    let unit = px_unit(spec, px);
    let r = if up { px.div_ceil(unit) * unit } else { px / unit * unit };
    let r = if r == 0 { unit } else { r };
    // Rounding up across a power of ten can only reduce the required
    // precision; rounding down can increase the digit count never. Re-check
    // and fall back to the unit of the result's magnitude.
    let p = Px(r);
    if spec.is_valid_px(p) {
        p
    } else {
        let u2 = px_unit(spec, r);
        Px(if up { r.div_ceil(u2) * u2 } else { (r / u2 * u2).max(u2) })
    }
}

/// `px * (1 + bps/10_000)` (bps may be negative), rounded to a valid price
/// in the given direction.
pub fn px_offset_bps(spec: &MarketSpec, px: Px, bps: i64, up: bool) -> Px {
    let raw = (px.0 as i128) * (10_000 + bps as i128) / 10_000;
    round_px(spec, raw.clamp(1, u64::MAX as i128 / 2) as u64, up)
}

/// One lot (raw Sz units) for the market.
pub fn lot(spec: &MarketSpec) -> i64 {
    10i64.pow(SZ_DECIMALS - spec.sz_decimals.min(SZ_DECIMALS))
}

/// Size (rounded down to lots, at least one lot) for a USD notional at `px`.
/// `usd` is raw (1e-6).
pub fn sz_for_notional(spec: &MarketSpec, usd: i128, px: Px) -> Sz {
    // notional_raw = px_raw * sz_raw / 1e10  =>  sz_raw = usd_raw * 1e10 / px_raw
    let raw = usd.max(0) * 10_000_000_000 / (px.0.max(1) as i128);
    let l = lot(spec) as i128;
    let v = (raw / l * l).max(l);
    Sz(v.min(i64::MAX as i128 / 4) as i64)
}

/// Size with notional at least `min_usd` (raw) at `px` (rounded up to lots).
pub fn min_sz_for_notional(spec: &MarketSpec, min_usd: i128, px: Px) -> Sz {
    let d = px.0.max(1) as i128;
    let raw = (min_usd * 10_000_000_000 + d - 1) / d;
    let l = lot(spec) as i128;
    Sz((((raw + l - 1) / l) * l).max(l) as i64)
}

/// Notional (raw USD 1e-6) of `sz` at `px`, rounded down.
pub fn notional_raw(px: Px, sz: Sz) -> i128 {
    (px.0 as i128) * (sz.0 as i128).abs() / 10_000_000_000
}

/// Size for `usd` notional, but never below the $10 minimum (+ margin).
pub fn order_sz(spec: &MarketSpec, usd: i128, px: Px) -> Sz {
    let s = sz_for_notional(spec, usd, px);
    let m = min_sz_for_notional(spec, 11 * USD_SCALE, px);
    if s.0 < m.0 {
        m
    } else {
        s
    }
}

/// Heavy-tailed (Pareto, alpha ~= 1) integer notional in raw USD within
/// `[min, max]`: `min * 2^k * (1 + U)` with `k ~ Geometric(1/2)`.
pub fn heavy_tail_usd<R: Rng + ?Sized>(rng: &mut R, min: i128, max: i128) -> i128 {
    let k = (rng.gen::<u32>() | (1 << 24)).trailing_ones().min(24);
    let base = min.saturating_mul(1i128 << k);
    let frac = rng.gen_range(0..1000) as i128;
    (base + base * frac / 1000).clamp(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rand::SeedableRng;

    fn spec(sz_decimals: u32) -> MarketSpec {
        MarketSpec {
            asset: 0,
            name: "X".into(),
            sz_decimals,
            max_leverage: 20,
            only_isolated: false,
            initial_px: Px(100 * PX_SCALE as u64),
        }
    }

    proptest! {
        #[test]
        fn rounded_prices_are_valid(px in 1u64..(1u64 << 52), szd in 0u32..6, up: bool) {
            let s = spec(szd);
            let p = round_px(&s, px, up);
            prop_assert!(s.is_valid_px(p), "{px} -> {p:?} szd {szd}");
            if up { prop_assert!(p.0 >= px); } else if p.0 > px { prop_assert!(p.0 == px_unit(&s, px)); }
        }

        #[test]
        fn sizes_are_valid(usd in 1i128..10_000_000_000_000i128, px in 1_000u64..(1u64 << 50), szd in 0u32..6) {
            let s = spec(szd);
            let sz = order_sz(&s, usd, Px(px));
            prop_assert!(s.is_valid_sz(sz));
            prop_assert!(notional_raw(Px(px), sz) >= 10 * USD_SCALE);
        }
    }

    #[test]
    fn heavy_tail_bounds() {
        let mut r = rand_chacha::ChaCha20Rng::seed_from_u64(1);
        let mut big = 0;
        for _ in 0..10_000 {
            let v = heavy_tail_usd(&mut r, 10_000_000, 1_000_000_000_000);
            assert!((10_000_000..=1_000_000_000_000).contains(&v));
            if v > 1_000_000_000 {
                big += 1;
            }
        }
        // P(> 100x min) ~ 1%: heavy tail present but rare.
        assert!(big > 10 && big < 500, "{big}");
    }
}
