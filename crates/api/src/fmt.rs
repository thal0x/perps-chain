//! HL-style number / id formatting.
//!
//! HL renders decimals as strings with trailing zeros trimmed but at least one
//! fractional digit for whole numbers ("1100.0", "0.0", "1891.4", "0.02").

use interfaces::*;

/// Format a fixed-point integer `v` with `decimals` implied decimals, HL style.
pub fn dec(v: i128, decimals: u32) -> String {
    let neg = v < 0;
    let a = v.unsigned_abs();
    let scale = 10u128.pow(decimals);
    let int = a / scale;
    let frac = a % scale;
    let mut s = String::with_capacity(24);
    if neg && a != 0 {
        s.push('-');
    }
    s.push_str(&int.to_string());
    s.push('.');
    if frac == 0 {
        s.push('0');
    } else {
        let f = format!("{:0width$}", frac, width = decimals as usize);
        s.push_str(f.trim_end_matches('0'));
    }
    s
}

pub fn px(p: Px) -> String {
    dec(p.raw(), PX_DECIMALS)
}
pub fn sz(s: Sz) -> String {
    dec(s.raw(), SZ_DECIMALS)
}
/// Absolute value of a size (for fields that are unsigned on the wire).
pub fn sz_abs(s: Sz) -> String {
    dec(s.raw().abs(), SZ_DECIMALS)
}
pub fn usd(u: Usd) -> String {
    dec(u.raw(), USD_DECIMALS)
}
pub fn usd_abs(u: Usd) -> String {
    dec(u.raw().abs(), USD_DECIMALS)
}
/// Non-negative USD field: negatives clamp to 0 (HL never shows negative there).
pub fn usd_nonneg(u: Usd) -> String {
    dec(u.raw().max(0), USD_DECIMALS)
}
pub fn rate(r: Rate) -> String {
    dec(r.raw(), RATE_DECIMALS)
}

pub fn addr(a: &Address) -> String {
    a.to_hex()
}
pub fn hash(h: &Hash32) -> String {
    h.to_hex()
}
pub fn cloid(c: &Cloid) -> String {
    format!("0x{}", hex::encode(c))
}

pub fn parse_cloid(s: &str) -> Option<Cloid> {
    let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    let v = hex::decode(h).ok()?;
    if v.len() != 16 {
        return None;
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&v);
    Some(out)
}

pub fn parse_addr(s: &str) -> Option<Address> {
    if !(s.starts_with("0x") || s.starts_with("0X")) || s.len() != 42 {
        return None;
    }
    Address::from_hex(s).ok()
}

/// Wall-clock ms (API-only; never used by the state machine).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Round a raw price (1e-8 units) to `n` significant figures (optionally with
/// mantissa `m` in the last figure), floor for bids and ceil for asks.
/// Integer-part digits beyond `n` are rounded too (HL l2Book aggregation).
pub fn round_sig(raw: u64, n: u32, mantissa: Option<u32>, up: bool) -> u64 {
    if raw == 0 || n == 0 {
        return raw;
    }
    let d = raw.to_string().len() as u32;
    if d <= n {
        return raw;
    }
    let mut unit = 10u128.pow(d - n);
    if let Some(m) = mantissa {
        if m > 1 {
            unit *= m as u128;
        }
    }
    let r = raw as u128;
    let q = r / unit;
    let rem = r % unit;
    let out = if up && rem != 0 { (q + 1) * unit } else { q * unit };
    u64::try_from(out).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dec_formats_like_hl() {
        assert_eq!(dec(0, 8), "0.0");
        assert_eq!(dec(110_000_000_000, 8), "1100.0");
        assert_eq!(dec(189_140_000_000, 8), "1891.4");
        assert_eq!(dec(2_000_000, 8), "0.02");
        assert_eq!(dec(-1_500_000, 6), "-1.5");
        assert_eq!(dec(12_500_000, 12), "0.0000125");
    }

    #[test]
    fn sig_rounding() {
        // 1891.43 with 2 sig figs -> 1800 (bid) / 1900 (ask)
        let raw = 189_143_000_000u64;
        assert_eq!(round_sig(raw, 2, None, false), 180_000_000_000);
        assert_eq!(round_sig(raw, 2, None, true), 190_000_000_000);
        // 5 sig figs mantissa 5: 1891.4 -> 1891.0 (bid, step 0.5 -> floor) ; ask 1891.5
        assert_eq!(round_sig(189_143_000_000, 5, Some(5), false), 189_100_000_000);
        assert_eq!(round_sig(189_143_000_000, 5, Some(5), true), 189_150_000_000);
        // small numbers unchanged when already within sig figs
        assert_eq!(round_sig(150_000_000, 5, None, false), 150_000_000);
    }

    #[test]
    fn cloid_roundtrip() {
        let c = parse_cloid("0x00000000000000000000000000000001").unwrap();
        assert_eq!(cloid(&c), "0x00000000000000000000000000000001");
        assert!(parse_cloid("0x01").is_none());
        assert!(parse_addr("0x0000000000000000000000000000000000000000").is_some());
        assert!(parse_addr("0x00").is_none());
    }
}
