//! `hlchaos`: fault-injection harness for hlbft (TEST_PLAN.md AC1).
//!
//! Drives real `hlnode` processes (docs/NODE_CLI.md): kill -9 / restart,
//! partitions, lossy links via `POST /admin/chaos`, a continuous safety
//! checker over `commits.log` / `roots.log` / `node.log`, and liveness
//! (resume-after-heal) measurement. See README.md.

pub mod checker;
pub mod cluster;
pub mod http;
pub mod load;
pub mod qc;
pub mod report;
pub mod runner;
pub mod schedule;
pub mod tail;
pub mod testutil;

/// Parse `12h`, `3m`, `40s`, `500ms`, `1h30m`, or a bare number of seconds.
pub fn parse_duration_ms(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(secs * 1000);
    }
    let mut total = 0u64;
    let mut num = String::new();
    let mut chars = s.chars().peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let mut unit = c.to_string();
        while let Some(&d) = chars.peek() {
            if d.is_ascii_alphabetic() {
                unit.push(d);
                chars.next();
            } else {
                break;
            }
        }
        let v: u64 = num.parse().map_err(|_| format!("bad duration {s:?}"))?;
        num.clear();
        let mult = match unit.as_str() {
            "ms" => 1,
            "s" => 1000,
            "m" | "min" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            _ => return Err(format!("bad duration unit {unit:?} in {s:?}")),
        };
        total += v * mult;
        any = true;
    }
    if !num.is_empty() || !any {
        return Err(format!("bad duration {s:?} (missing unit)"));
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("12h"), Ok(43_200_000));
        assert_eq!(parse_duration_ms("3m"), Ok(180_000));
        assert_eq!(parse_duration_ms("40s"), Ok(40_000));
        assert_eq!(parse_duration_ms("500ms"), Ok(500));
        assert_eq!(parse_duration_ms("1h30m"), Ok(5_400_000));
        assert_eq!(parse_duration_ms("90"), Ok(90_000));
        assert!(parse_duration_ms("5x").is_err());
        assert!(parse_duration_ms("1h5").is_err());
        assert!(parse_duration_ms("").is_err());
    }
}
