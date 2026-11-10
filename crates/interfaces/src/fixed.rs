//! Deterministic fixed-point numbers. **No floating point is allowed anywhere
//! in the state machine** (consensus, book, clearinghouse, oracle, state).
//!
//! | type  | repr | scale | meaning                                  |
//! |-------|------|-------|------------------------------------------|
//! | `Px`  | u64  | 1e-8  | price in USD per unit of the asset        |
//! | `Sz`  | i64  | 1e-8  | asset quantity; signed for positions      |
//! | `Usd` | i64  | 1e-6  | USDC amount (USDC native 6 decimals)      |
//! | `Rate`| i64  | 1e-12 | dimensionless ratio (funding, fees, margin)|
//!
//! Intermediate products use i128. Every operation that can lose precision
//! takes an explicit [`Rounding`]; there is no implicit rounding.

use crate::error::ParseError;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const PX_DECIMALS: u32 = 8;
pub const SZ_DECIMALS: u32 = 8;
pub const USD_DECIMALS: u32 = 6;
pub const RATE_DECIMALS: u32 = 12;

pub const PX_SCALE: i128 = 100_000_000;
pub const SZ_SCALE: i128 = 100_000_000;
pub const USD_SCALE: i128 = 1_000_000;
pub const RATE_SCALE: i128 = 1_000_000_000_000;

/// Explicit rounding mode for every lossy operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rounding {
    /// toward -infinity
    Floor,
    /// toward +infinity
    Ceil,
    /// toward zero
    TowardZero,
    /// away from zero
    AwayFromZero,
}

/// `a * b / d` computed in i128 with explicit rounding. Panics on d == 0 or
/// on overflow of the i128 intermediate (state machine callers must keep
/// operands within documented bounds; overflow is a bug, not an input error).
pub fn mul_div(a: i128, b: i128, d: i128, r: Rounding) -> i128 {
    assert!(d != 0, "mul_div by zero");
    let n = a.checked_mul(b).expect("mul_div overflow");
    div_round(n, d, r)
}

/// `n / d` with explicit rounding.
pub fn div_round(n: i128, d: i128, r: Rounding) -> i128 {
    assert!(d != 0, "div by zero");
    let q = n / d; // truncates toward zero
    let rem = n % d;
    if rem == 0 {
        return q;
    }
    let negative = (n < 0) != (d < 0);
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

/// Parse an exact decimal string ("123", "0.25", "-1.5", "1e-3" not allowed)
/// into an integer with `decimals` implied decimal places. Rejects strings
/// with more fractional precision than `decimals` (after trimming trailing
/// zeros) instead of rounding.
pub fn parse_decimal(s: &str, decimals: u32) -> Result<i128, ParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ParseError::Empty);
    }
    let (neg, body) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let (int_part, frac_part) = match body.find('.') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None => (body, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(ParseError::BadDecimal(s.to_string()));
    }
    if !int_part.bytes().all(|c| c.is_ascii_digit()) || !frac_part.bytes().all(|c| c.is_ascii_digit()) {
        return Err(ParseError::BadDecimal(s.to_string()));
    }
    let frac_trim = frac_part.trim_end_matches('0');
    if frac_trim.len() as u32 > decimals {
        return Err(ParseError::TooPrecise(s.to_string()));
    }
    let mut v: i128 = 0;
    for c in int_part.bytes() {
        v = v
            .checked_mul(10)
            .and_then(|x| x.checked_add((c - b'0') as i128))
            .ok_or(ParseError::Overflow)?;
    }
    for i in 0..decimals as usize {
        let d = frac_trim.as_bytes().get(i).map(|c| (c - b'0') as i128).unwrap_or(0);
        v = v.checked_mul(10).and_then(|x| x.checked_add(d)).ok_or(ParseError::Overflow)?;
    }
    Ok(if neg { -v } else { v })
}

/// Format an integer with `decimals` implied places as a minimal decimal
/// string: trailing fractional zeros trimmed, no trailing '.', "-0" never
/// produced. This matches HL `float_to_wire` normalisation (e.g. 1100 -> "1100",
/// 0.2 -> "0.2"). API code that wants "1100.0" style must post-process.
pub fn format_decimal(v: i128, decimals: u32) -> String {
    let neg = v < 0;
    let a = v.unsigned_abs();
    let scale = 10u128.pow(decimals);
    let int = a / scale;
    let frac = a % scale;
    let mut s = String::new();
    if neg && a != 0 {
        s.push('-');
    }
    s.push_str(&int.to_string());
    if frac != 0 {
        let f = format!("{:0width$}", frac, width = decimals as usize);
        s.push('.');
        s.push_str(f.trim_end_matches('0'));
    }
    s
}

macro_rules! fixed_type {
    ($name:ident, $repr:ty, $decimals:expr, $doc:expr) => {
        #[doc = $doc]
        #[derive(
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Default,
            BorshSerialize,
            BorshDeserialize,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub $repr);

        impl $name {
            pub const ZERO: Self = Self(0);
            pub const DECIMALS: u32 = $decimals;
            pub const ONE: Self = Self((10 as $repr).pow($decimals));

            /// Exact parse of a decimal wire string; errors if too precise.
            pub fn parse(s: &str) -> Result<Self, ParseError> {
                let v = parse_decimal(s, $decimals)?;
                <$repr>::try_from(v).map(Self).map_err(|_| ParseError::Overflow)
            }
            /// Minimal decimal string ("0.2", "1100").
            pub fn to_wire(&self) -> String {
                format_decimal(self.0 as i128, $decimals)
            }
            pub fn raw(&self) -> i128 {
                self.0 as i128
            }
            pub fn from_raw(v: i128) -> Self {
                Self(<$repr>::try_from(v).expect(concat!(stringify!($name), " overflow")))
            }
            pub fn checked_add(self, o: Self) -> Option<Self> {
                self.0.checked_add(o.0).map(Self)
            }
            pub fn checked_sub(self, o: Self) -> Option<Self> {
                self.0.checked_sub(o.0).map(Self)
            }
            pub fn is_zero(&self) -> bool {
                self.0 == 0
            }
            /// Round to a multiple of 10^(DECIMALS - keep_decimals).
            pub fn round_to_decimals(self, keep_decimals: u32, r: Rounding) -> Self {
                assert!(keep_decimals <= $decimals);
                let unit = 10i128.pow($decimals - keep_decimals);
                Self::from_raw(div_round(self.0 as i128, unit, r) * unit)
            }
            /// True if the value has at most `keep_decimals` fractional digits.
            pub fn has_at_most_decimals(&self, keep_decimals: u32) -> bool {
                if keep_decimals >= $decimals {
                    return true;
                }
                (self.0 as i128) % 10i128.pow($decimals - keep_decimals) == 0
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.to_wire())
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.to_wire())
            }
        }
    };
}

fixed_type!(Px, u64, PX_DECIMALS, "Price, 1e-8 USD per asset unit. Always > 0 for orders.");
fixed_type!(Sz, i64, SZ_DECIMALS, "Asset quantity, 1e-8 units. Signed: + long, - short.");
fixed_type!(Usd, i64, USD_DECIMALS, "USDC amount, 1e-6 USD.");
fixed_type!(Rate, i64, RATE_DECIMALS, "Dimensionless ratio, 1e-12.");

macro_rules! signed_ops {
    ($name:ident) => {
        impl std::ops::Add for $name {
            type Output = Self;
            fn add(self, o: Self) -> Self {
                Self(self.0.checked_add(o.0).expect(concat!(stringify!($name), " add overflow")))
            }
        }
        impl std::ops::Sub for $name {
            type Output = Self;
            fn sub(self, o: Self) -> Self {
                Self(self.0.checked_sub(o.0).expect(concat!(stringify!($name), " sub overflow")))
            }
        }
        impl std::ops::Neg for $name {
            type Output = Self;
            fn neg(self) -> Self {
                Self(self.0.checked_neg().expect("neg overflow"))
            }
        }
        impl std::ops::AddAssign for $name {
            fn add_assign(&mut self, o: Self) {
                *self = *self + o;
            }
        }
        impl std::ops::SubAssign for $name {
            fn sub_assign(&mut self, o: Self) {
                *self = *self - o;
            }
        }
        impl $name {
            pub fn abs(self) -> Self {
                Self(self.0.checked_abs().expect("abs overflow"))
            }
            pub fn signum(self) -> i64 {
                self.0.signum() as i64
            }
            pub fn min(self, o: Self) -> Self {
                if self <= o {
                    self
                } else {
                    o
                }
            }
            pub fn max(self, o: Self) -> Self {
                if self >= o {
                    self
                } else {
                    o
                }
            }
        }
    };
}
signed_ops!(Sz);
signed_ops!(Usd);
signed_ops!(Rate);

impl std::ops::Add for Px {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self(self.0.checked_add(o.0).expect("Px add overflow"))
    }
}
impl std::ops::Sub for Px {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        Self(self.0.checked_sub(o.0).expect("Px sub underflow"))
    }
}

/// Notional value `px * |sz|`-style product: `px * sz` in USD, sign follows `sz`.
pub fn notional(px: Px, sz: Sz, r: Rounding) -> Usd {
    // px(1e8) * sz(1e8) = 1e16 scale; USD is 1e6 -> divide by 1e10.
    Usd::from_raw(mul_div(px.0 as i128, sz.0 as i128, PX_SCALE * SZ_SCALE / USD_SCALE, r))
}

/// `usd * rate` with explicit rounding.
pub fn usd_mul_rate(u: Usd, rate: Rate, r: Rounding) -> Usd {
    Usd::from_raw(mul_div(u.0 as i128, rate.0 as i128, RATE_SCALE, r))
}

/// `px * rate` (e.g. price impact, premium).
pub fn px_mul_rate(p: Px, rate: Rate, r: Rounding) -> Px {
    Px::from_raw(mul_div(p.0 as i128, rate.0 as i128, RATE_SCALE, r))
}

/// `a / b` as a Rate. Panics if b == 0.
pub fn usd_ratio(a: Usd, b: Usd, r: Rounding) -> Rate {
    Rate::from_raw(mul_div(a.0 as i128, RATE_SCALE, b.0 as i128, r))
}

/// Price ratio `(a - b) / b` as a Rate.
pub fn px_rel_diff(a: Px, b: Px, r: Rounding) -> Rate {
    Rate::from_raw(mul_div(a.0 as i128 - b.0 as i128, RATE_SCALE, b.0 as i128, r))
}

/// Number of significant figures in a price (HL allows at most 5 unless the
/// price is an integer).
pub fn px_sig_figs(p: Px) -> u32 {
    let mut v = p.0;
    if v == 0 {
        return 0;
    }
    while v % 10 == 0 {
        v /= 10;
    }
    let mut n = 0;
    while v > 0 {
        v /= 10;
        n += 1;
    }
    n
}

/// Size quotient `usd / px` in Sz units.
pub fn sz_from_usd(u: Usd, p: Px, r: Rounding) -> Sz {
    Sz::from_raw(mul_div(u.0 as i128, PX_SCALE * SZ_SCALE / USD_SCALE, p.0 as i128, r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format_roundtrip() {
        assert_eq!(Px::parse("1100").unwrap().0, 1100 * 100_000_000);
        assert_eq!(Sz::parse("0.2").unwrap().0, 20_000_000);
        assert_eq!(Sz::parse("-0.2").unwrap().0, -20_000_000);
        assert_eq!(Usd::parse("1.000000").unwrap().0, 1_000_000);
        assert!(Usd::parse("1.0000001").is_err());
        assert_eq!(Px::parse("1100").unwrap().to_wire(), "1100");
        assert_eq!(Sz::parse("0.0200").unwrap().to_wire(), "0.02");
        assert_eq!(Usd::from_raw(0).to_wire(), "0");
        assert_eq!(Usd::from_raw(-5).to_wire(), "-0.000005");
        assert!(Px::parse("-1").is_err());
        assert!(Px::parse("abc").is_err());
        assert!(Px::parse(".").is_err());
        assert_eq!(Px::parse(".5").unwrap().to_wire(), "0.5");
    }

    #[test]
    fn rounding_modes() {
        assert_eq!(div_round(7, 2, Rounding::Floor), 3);
        assert_eq!(div_round(-7, 2, Rounding::Floor), -4);
        assert_eq!(div_round(7, 2, Rounding::Ceil), 4);
        assert_eq!(div_round(-7, 2, Rounding::Ceil), -3);
        assert_eq!(div_round(-7, 2, Rounding::TowardZero), -3);
        assert_eq!(div_round(-7, 2, Rounding::AwayFromZero), -4);
        assert_eq!(div_round(6, 2, Rounding::AwayFromZero), 3);
    }

    #[test]
    fn notional_math() {
        let px = Px::parse("2000.5").unwrap();
        let sz = Sz::parse("0.1").unwrap();
        assert_eq!(notional(px, sz, Rounding::Floor).to_wire(), "200.05");
        assert_eq!(notional(px, -sz, Rounding::Floor).to_wire(), "-200.05");
        assert_eq!(px_sig_figs(Px::parse("12345").unwrap()), 5);
        assert_eq!(px_sig_figs(Px::parse("1234.56").unwrap()), 6);
        assert_eq!(px_sig_figs(Px::parse("0.0001234").unwrap()), 4);
    }
}
