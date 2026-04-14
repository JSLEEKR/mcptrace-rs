//! Duration parser for SLO window strings like `"5m"`, `"1h"`, `"30d"`.
//!
//! We intentionally keep this tiny rather than pull in `humantime` or
//! `parse_duration` — a single regex-free loop is auditable and has no
//! surprises. The grammar is:
//!
//! ```text
//! duration = [0-9]+ ("ns"|"us"|"ms"|"s"|"m"|"h"|"d")
//! ```
//!
//! Returned as nanoseconds (`u128`) for direct comparison with span
//! `start_unix_nanos` fields.

use crate::error::{Error, Result};

/// Parse a duration string into nanoseconds.
pub fn parse_duration_nanos(s: &str) -> Result<u128> {
    let s = s.trim();
    if s.is_empty() {
        return Err(Error::InvalidDuration("empty string".into()));
    }

    // Split trailing non-digit suffix (max 2 chars: ns/us/ms).
    let split_idx = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| Error::InvalidDuration(format!("no unit suffix: {s}")))?;

    if split_idx == 0 {
        return Err(Error::InvalidDuration(format!("missing number: {s}")));
    }

    let (num_str, unit) = s.split_at(split_idx);
    let n: u128 = num_str
        .parse()
        .map_err(|_| Error::InvalidDuration(format!("bad number: {num_str}")))?;

    let factor: u128 = match unit {
        "ns" => 1,
        "us" => 1_000,
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "m" => 60 * 1_000_000_000,
        "h" => 3_600 * 1_000_000_000,
        "d" => 86_400 * 1_000_000_000,
        other => {
            return Err(Error::InvalidDuration(format!("unknown unit: {other}")));
        }
    };
    let nanos = n
        .checked_mul(factor)
        .ok_or_else(|| Error::InvalidDuration(format!("overflow: {s}")))?;

    Ok(nanos)
}

/// Format a nanosecond count as an approximate human-readable string.
/// Used only for log/alert output; never as a round-trip format.
#[must_use]
pub fn format_nanos_human(nanos: u128) -> String {
    if nanos >= 86_400 * 1_000_000_000 {
        format!("{}d", nanos / (86_400 * 1_000_000_000))
    } else if nanos >= 3_600 * 1_000_000_000 {
        format!("{}h", nanos / (3_600 * 1_000_000_000))
    } else if nanos >= 60 * 1_000_000_000 {
        format!("{}m", nanos / (60 * 1_000_000_000))
    } else if nanos >= 1_000_000_000 {
        format!("{}s", nanos / 1_000_000_000)
    } else if nanos >= 1_000_000 {
        format!("{}ms", nanos / 1_000_000)
    } else {
        format!("{nanos}ns")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_5m() {
        assert_eq!(parse_duration_nanos("5m").unwrap(), 5 * 60 * 1_000_000_000);
    }

    #[test]
    fn parse_1h() {
        assert_eq!(
            parse_duration_nanos("1h").unwrap(),
            3_600 * 1_000_000_000
        );
    }

    #[test]
    fn parse_30d() {
        assert_eq!(
            parse_duration_nanos("30d").unwrap(),
            30 * 86_400 * 1_000_000_000
        );
    }

    #[test]
    fn parse_seconds() {
        assert_eq!(parse_duration_nanos("1s").unwrap(), 1_000_000_000);
    }

    #[test]
    fn parse_ms() {
        assert_eq!(parse_duration_nanos("500ms").unwrap(), 500_000_000);
    }

    #[test]
    fn parse_us() {
        assert_eq!(parse_duration_nanos("250us").unwrap(), 250_000);
    }

    #[test]
    fn parse_ns() {
        assert_eq!(parse_duration_nanos("7ns").unwrap(), 7);
    }

    #[test]
    fn parse_empty_fails() {
        assert!(parse_duration_nanos("").is_err());
        assert!(parse_duration_nanos("   ").is_err());
    }

    #[test]
    fn parse_missing_unit_fails() {
        assert!(parse_duration_nanos("5").is_err());
    }

    #[test]
    fn parse_unknown_unit_fails() {
        assert!(parse_duration_nanos("5y").is_err());
    }

    #[test]
    fn parse_missing_number_fails() {
        assert!(parse_duration_nanos("m").is_err());
    }

    #[test]
    fn parse_negative_fails() {
        // - is not a digit, so find() stops at 0 -> no number
        assert!(parse_duration_nanos("-5m").is_err());
    }

    #[test]
    fn parse_huge_overflows() {
        assert!(parse_duration_nanos("340282366920938463463374607431768211455d").is_err());
    }

    #[test]
    fn parse_zero_ok() {
        assert_eq!(parse_duration_nanos("0s").unwrap(), 0);
    }

    #[test]
    fn format_small() {
        assert_eq!(format_nanos_human(500), "500ns");
    }

    #[test]
    fn format_ms_range() {
        assert_eq!(format_nanos_human(5_000_000), "5ms");
    }

    #[test]
    fn format_s_range() {
        assert_eq!(format_nanos_human(2_500_000_000), "2s");
    }

    #[test]
    fn format_m_range() {
        assert_eq!(format_nanos_human(120 * 1_000_000_000), "2m");
    }

    #[test]
    fn format_h_range() {
        assert_eq!(format_nanos_human(3 * 3_600 * 1_000_000_000), "3h");
    }

    #[test]
    fn format_d_range() {
        assert_eq!(format_nanos_human(2 * 86_400 * 1_000_000_000), "2d");
    }

    #[test]
    fn parse_with_whitespace() {
        assert_eq!(parse_duration_nanos("  5m  ").unwrap(), 5 * 60 * 1_000_000_000);
    }
}
