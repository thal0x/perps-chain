//! Fixed-point helpers used by the clearinghouse. Everything is integer
//! arithmetic with explicit rounding; there is no floating point.

use interfaces::fixed::*;
use interfaces::market::MarketSpec;

/// `a * b / d` with a 256-bit intermediate (never overflows unless the
/// final quotient does not fit an i128). Explicit rounding.
pub fn mul_div_wide(a: i128, b: i128, d: i128, r: Rounding) -> i128 {
    assert!(d != 0, "mul_div_wide by zero");
    if let Some(n) = a.checked_mul(b) {
        return div_round(n, d, r);
    }
    let negative = ((a < 0) != (b < 0)) != (d < 0);
    let (hi, lo) = mul_u128(a.unsigned_abs(), b.unsigned_abs());
    let (q, rem_nonzero) = div_256_by_128(hi, lo, d.unsigned_abs());
    let q = i128::try_from(q).expect("mul_div_wide quotient overflow");
    let q = if negative { -q } else { q };
    if !rem_nonzero {
        return q;
    }
    match r {
        Rounding::TowardZero => q,
        Rounding::AwayFromZero => {
            if negative {
                q - 1
            } else {
                q + 1
            }
        }
        Rounding::Floor => {
            if negative {
                q - 1
            } else {
                q
            }
        }
        Rounding::Ceil => {
            if negative {
                q
            } else {
                q + 1
            }
        }
    }
}

/// Full 128x128 -> 256 bit unsigned product as (hi, lo).
fn mul_u128(a: u128, b: u128) -> (u128, u128) {
    const M: u128 = u64::MAX as u128;
    let (a1, a0) = (a >> 64, a & M);
    let (b1, b0) = (b >> 64, b & M);
    let p00 = a0 * b0;
    let p01 = a0 * b1;
    let p10 = a1 * b0;
    let p11 = a1 * b1;
    let mid = (p00 >> 64) + (p01 & M) + (p10 & M);
    let lo = (p00 & M) | ((mid & M) << 64);
    let hi = p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64);
    (hi, lo)
}

/// (hi:lo) / d by binary long division. d < 2^127. Returns (quotient
/// (must fit u128), remainder != 0).
fn div_256_by_128(hi: u128, lo: u128, d: u128) -> (u128, bool) {
    assert!(d > 0 && d < (1u128 << 127));
    assert!(hi < d, "mul_div_wide quotient overflow");
    let mut rem = hi;
    let mut q: u128 = 0;
    for i in (0..128).rev() {
        rem = (rem << 1) | ((lo >> i) & 1);
        q <<= 1;
        if rem >= d {
            rem -= d;
            q |= 1;
        }
    }
    (q, rem != 0)
}

/// `ceil(|u| / lev)` for a non-negative notional.
pub fn div_lev_ceil(u: Usd, lev: u32) -> Usd {
    Usd::from_raw(div_round(u.raw(), lev as i128, Rounding::Ceil))
}

/// Exact `px * szi * rate` in Usd with rounding `r` (wide intermediate).
pub fn px_sz_rate(px: Px, szi: Sz, rate: Rate, r: Rounding) -> Usd {
    let n = px.raw() * szi.raw(); // scale 1e16, <= ~1e27
    Usd::from_raw(mul_div_wide(n, rate.raw(), PX_SCALE * SZ_SCALE / USD_SCALE * RATE_SCALE, r))
}

/// `usd * a / b` with rounding.
pub fn usd_mul_frac(u: Usd, a: i128, b: i128, r: Rounding) -> Usd {
    Usd::from_raw(mul_div_wide(u.raw(), a, b, r))
}

/// Price per unit for a cash amount `u` over size `sz` (both magnitudes),
/// rounded with `r`. Returns Px(0) if sz == 0.
pub fn px_from_ntl(u: Usd, sz: Sz, r: Rounding) -> Px {
    if sz.is_zero() {
        return Px::ZERO;
    }
    let v = mul_div_wide(u.raw().abs(), PX_SCALE * SZ_SCALE / USD_SCALE, sz.raw().abs(), r);
    Px::from_raw(v.min(u64::MAX as i128))
}

/// Round `px` to a price that satisfies the market's tick rules (at most
/// `6 - sz_decimals` decimals; at most 5 significant figures unless an
/// integer). `up` selects the rounding direction. Never returns 0.
pub fn round_px_valid(spec: &MarketSpec, px: Px, up: bool) -> Px {
    let max_dec = spec.max_px_decimals().min(PX_DECIMALS);
    let dec_unit: u64 = 10u64.pow(PX_DECIMALS - max_dec);
    // e = index of most significant digit of the raw value
    let mut e: u32 = 0;
    let mut v = px.0;
    while v >= 10 {
        v /= 10;
        e += 1;
    }
    let sig_unit: u64 = if e >= 4 { 10u64.pow((e - 4).min(PX_DECIMALS)) } else { 1 };
    let unit = dec_unit.max(sig_unit);
    let r = if up { Rounding::Ceil } else { Rounding::Floor };
    let q = div_round(px.0 as i128, unit as i128, r) * unit as i128;
    let q = if q <= 0 { unit as i128 } else { q };
    Px::from_raw(q)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_matches_narrow() {
        for (a, b, d) in [(7i128, 3i128, 2i128), (-7, 3, 2), (10, -10, 3), (123456789, 987654321, 1000)] {
            for r in [Rounding::Floor, Rounding::Ceil, Rounding::TowardZero, Rounding::AwayFromZero] {
                assert_eq!(mul_div_wide(a, b, d, r), mul_div(a, b, d, r));
            }
        }
    }

    #[test]
    fn wide_large() {
        let a: i128 = 10i128.pow(27);
        let b: i128 = 4 * 10i128.pow(10);
        let d: i128 = 10i128.pow(22);
        assert_eq!(mul_div_wide(a, b, d, Rounding::Floor), 4 * 10i128.pow(15));
        assert_eq!(mul_div_wide(-a - 1, b, d, Rounding::Floor), -4 * 10i128.pow(15) - 1);
        assert_eq!(mul_div_wide(-a - 1, b, d, Rounding::Ceil), -4 * 10i128.pow(15));
        assert_eq!(mul_div_wide(a + 1, b, d, Rounding::Ceil), 4 * 10i128.pow(15) + 1);
        // 2^100 * 2^100 / 2^90 = 2^110
        assert_eq!(mul_div_wide(1i128 << 100, 1i128 << 100, 1i128 << 90, Rounding::Floor), 1i128 << 110);
    }

    #[test]
    fn px_rounding() {
        let spec = MarketSpec {
            asset: 0,
            name: "BTC".into(),
            sz_decimals: 5,
            max_leverage: 50,
            only_isolated: false,
            initial_px: Px::parse("100000").unwrap(),
        };
        // 5 sig figs / 1 decimal
        assert_eq!(round_px_valid(&spec, Px::parse("98765.4321").unwrap(), true), Px::parse("98766").unwrap());
        assert_eq!(round_px_valid(&spec, Px::parse("98765.4321").unwrap(), false), Px::parse("98765").unwrap());
        assert_eq!(round_px_valid(&spec, Px::parse("102000.37").unwrap(), false), Px::parse("102000").unwrap());
        assert_eq!(round_px_valid(&spec, Px::parse("1234.56").unwrap(), true), Px::parse("1234.6").unwrap());
        let eth = MarketSpec { sz_decimals: 4, ..spec.clone() };
        assert_eq!(round_px_valid(&eth, Px::parse("1.234567").unwrap(), false), Px::parse("1.23").unwrap());
        assert!(eth.is_valid_px(round_px_valid(&eth, Px::parse("3333.3333").unwrap(), true)));
    }
}
