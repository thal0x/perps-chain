//! Candle intervals and bucket arithmetic (UTC).

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Interval {
    M1,
    M3,
    M5,
    M15,
    M30,
    H1,
    H2,
    H4,
    H8,
    H12,
    D1,
    D3,
    W1,
    Mo1,
}

pub const ALL_INTERVALS: [Interval; 14] = [
    Interval::M1,
    Interval::M3,
    Interval::M5,
    Interval::M15,
    Interval::M30,
    Interval::H1,
    Interval::H2,
    Interval::H4,
    Interval::H8,
    Interval::H12,
    Interval::D1,
    Interval::D3,
    Interval::W1,
    Interval::Mo1,
];

const MIN: u64 = 60_000;
const HOUR: u64 = 60 * MIN;
const DAY: u64 = 24 * HOUR;

impl Interval {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "1m" => Interval::M1,
            "3m" => Interval::M3,
            "5m" => Interval::M5,
            "15m" => Interval::M15,
            "30m" => Interval::M30,
            "1h" => Interval::H1,
            "2h" => Interval::H2,
            "4h" => Interval::H4,
            "8h" => Interval::H8,
            "12h" => Interval::H12,
            "1d" => Interval::D1,
            "3d" => Interval::D3,
            "1w" => Interval::W1,
            "1M" => Interval::Mo1,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Interval::M1 => "1m",
            Interval::M3 => "3m",
            Interval::M5 => "5m",
            Interval::M15 => "15m",
            Interval::M30 => "30m",
            Interval::H1 => "1h",
            Interval::H2 => "2h",
            Interval::H4 => "4h",
            Interval::H8 => "8h",
            Interval::H12 => "12h",
            Interval::D1 => "1d",
            Interval::D3 => "3d",
            Interval::W1 => "1w",
            Interval::Mo1 => "1M",
        }
    }

    fn fixed_ms(self) -> Option<u64> {
        Some(match self {
            Interval::M1 => MIN,
            Interval::M3 => 3 * MIN,
            Interval::M5 => 5 * MIN,
            Interval::M15 => 15 * MIN,
            Interval::M30 => 30 * MIN,
            Interval::H1 => HOUR,
            Interval::H2 => 2 * HOUR,
            Interval::H4 => 4 * HOUR,
            Interval::H8 => 8 * HOUR,
            Interval::H12 => 12 * HOUR,
            Interval::D1 => DAY,
            Interval::D3 => 3 * DAY,
            Interval::W1 | Interval::Mo1 => return None,
        })
    }

    /// (open time, close time inclusive) of the bucket containing `t` (ms).
    pub fn bucket(self, t: u64) -> (u64, u64) {
        if let Some(len) = self.fixed_ms() {
            let s = t - t % len;
            return (s, s + len - 1);
        }
        match self {
            Interval::W1 => {
                // Weeks start Monday 00:00 UTC (1970-01-01 was a Thursday).
                let off = 3 * DAY;
                let w = 7 * DAY;
                let s = (t + off) / w * w;
                let s = s.saturating_sub(off);
                (s, s + w - 1)
            }
            _ => {
                let days = (t / DAY) as i64;
                let (y, m, _) = civil_from_days(days);
                let start = days_from_civil(y, m, 1) as u64 * DAY;
                let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
                let next = days_from_civil(ny, nm, 1) as u64 * DAY;
                (start, next - 1)
            }
        }
    }
}

/// Howard Hinnant's days_from_civil.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// "YYYY-MM-DD" of a ms timestamp.
pub fn date_str(t: u64) -> String {
    let (y, m, d) = civil_from_days((t / DAY) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets() {
        // 2024-03-15T12:34:56.789Z = 1710506096789
        let t = 1_710_506_096_789u64;
        assert_eq!(Interval::M1.bucket(t), (1_710_506_040_000, 1_710_506_099_999));
        assert_eq!(Interval::H1.bucket(t).0, 1_710_504_000_000);
        assert_eq!(Interval::D1.bucket(t).0, 1_710_460_800_000);
        // Monday 2024-03-11
        assert_eq!(Interval::W1.bucket(t).0, 1_710_115_200_000);
        // 2024-03-01 .. 2024-03-31T23:59:59.999
        assert_eq!(Interval::Mo1.bucket(t), (1_709_251_200_000, 1_711_929_599_999));
        assert_eq!(date_str(t), "2024-03-15");
        for i in ALL_INTERVALS {
            assert_eq!(Interval::parse(i.as_str()), Some(i));
            let (s, e) = i.bucket(t);
            assert!(s <= t && t <= e);
        }
    }
}
