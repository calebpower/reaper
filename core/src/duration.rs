//! Duration strings, as they appear in manifests and site configuration.
//!
//! `30s`, `10m`, `2h`, `7d`. Deliberately narrow: a TTL is a human-scale
//! quantity and accepting `1.5h` or `90 minutes` buys nothing but ways to be
//! surprised.

use std::fmt;
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
pub struct ParseError {
    input: String,
    reason: &'static str,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} is not a duration ({}); expected a whole number followed by s, m, h or d",
            self.input, self.reason
        )
    }
}

impl std::error::Error for ParseError {}

/// Parse a duration string. Zero is rejected: every duration in this project is
/// a deadline or a cadence, and both mean something else entirely at zero.
pub fn parse(s: &str) -> Result<Duration, ParseError> {
    let err = |reason| ParseError {
        input: s.to_string(),
        reason,
    };

    // Split before the final *character*, not the final byte: a multi-byte
    // unit like "5µ" must be an unknown-unit error, not a panic on a byte
    // index that is not a char boundary.
    let unit_len = s.chars().last().map(char::len_utf8).unwrap_or(0);
    let (digits, unit) = s.split_at(s.len() - unit_len);
    let secs_per = match unit {
        "s" => 1u64,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => return Err(err("unknown or missing unit")),
    };

    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(err("the amount is not a whole number"));
    }

    let amount: u64 = digits.parse().map_err(|_| err("the amount is too large"))?;
    if amount == 0 {
        return Err(err("zero"));
    }

    // Ten years is the ceiling. Every duration in this project is a deadline
    // or a cadence; a value above this is a typo, and refusing it here is
    // what keeps every `SystemTime + ttl` downstream from overflowing.
    const MAX: u64 = 3650 * 24 * 60 * 60;
    let secs = amount
        .checked_mul(secs_per)
        .ok_or_else(|| err("the amount is too large"))?;
    if secs > MAX {
        return Err(err("longer than ten years, which is a typo, not a plan"));
    }
    Ok(Duration::from_secs(secs))
}

/// Render a duration the way a person reads a remaining-time column: the
/// largest unit that divides it exactly, so `2h` round-trips rather than
/// becoming `7200s`.
pub fn format(d: Duration) -> String {
    let s = d.as_secs();
    for (unit, per) in [("d", 86_400u64), ("h", 3_600), ("m", 60)] {
        if s % per == 0 && s >= per {
            return format!("{}{unit}", s / per);
        }
    }
    format!("{s}s")
}

/// Render a duration approximately, for display only -- `1h47m`, `3m12s`.
/// Never round-trips, and is not used anywhere a value is stored.
pub fn format_rough(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_unit() {
        assert_eq!(parse("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse("7d").unwrap(), Duration::from_secs(604_800));
    }

    #[test]
    fn rejects_what_it_should() {
        // Each of these is a plausible thing to type, and each would mean
        // something the caller did not intend if it were quietly accepted.
        for bad in ["", "h", "2", "1.5h", "90 minutes", "-1h", "2H", "0h", "0s", "2x"] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn rejects_overflow_rather_than_wrapping() {
        assert!(parse("99999999999999999999d").is_err());
    }

    #[test]
    fn exact_durations_round_trip() {
        for s in ["30s", "10m", "2h", "7d"] {
            assert_eq!(format(parse(s).unwrap()), s, "{s} should round-trip");
        }
    }

    #[test]
    fn rough_formatting_is_for_reading_only() {
        assert_eq!(format_rough(Duration::from_secs(45)), "45s");
        assert_eq!(format_rough(Duration::from_secs(192)), "3m12s");
        assert_eq!(format_rough(Duration::from_secs(6420)), "1h47m");
    }
}
