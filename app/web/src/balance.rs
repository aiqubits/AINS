//! Balance display utilities shared across views.
//!
//! The balance is stored as a "big value" in the database:
//! 1 display unit = 10^10 stored units.

/// Balance scale factor: 1 display unit = 10^10 stored units (1 × 10^10).
pub const BALANCE_SCALE: i64 = 10_000_000_000;

/// Format stored balance to display string, truncated to 2 decimal places.
pub fn format_balance(stored: i64) -> String {
    // Divide by 10^8 to get value in cents (100 per display unit), truncating extra decimals
    let cents = stored / (BALANCE_SCALE / 100);
    let sign = if cents < 0 { "-" } else { "" };
    let abs_cents = cents.unsigned_abs();
    let integer = abs_cents / 100;
    let fraction = abs_cents % 100;
    format!("{}{}.{:02}", sign, integer, fraction)
}

/// Parse a display-unit decimal string (e.g. "10.5") into stored units
/// (1 display unit = 10^10 stored units).
///
/// Exact decimal parsing — no f64 round-trip — so the full i64 stored range
/// is representable without silent precision loss (f64 is only exact up to
/// 2^53 stored units ≈ 9×10^5 display units). Rejects negative values,
/// scientific notation, and more than 10 fractional digits (beyond stored
/// precision). Returns `None` on any invalid input or overflow.
pub fn parse_display_amount(input: &str) -> Option<i64> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    if frac_part.len() > 10 {
        return None;
    }
    let int_val: i64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().ok()?
    };
    // Right-pad the fraction to exactly 10 digits (the stored precision).
    let mut frac = String::with_capacity(10);
    frac.push_str(frac_part);
    while frac.len() < 10 {
        frac.push('0');
    }
    let frac_val: i64 = frac.parse().ok()?;
    int_val.checked_mul(BALANCE_SCALE)?.checked_add(frac_val)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_balance_zero() {
        assert_eq!(format_balance(0), "0.00");
    }

    #[test]
    fn test_format_balance_one_unit() {
        assert_eq!(format_balance(BALANCE_SCALE), "1.00");
    }

    #[test]
    fn test_format_balance_half_unit() {
        assert_eq!(format_balance(BALANCE_SCALE / 2), "0.50");
    }

    #[test]
    fn test_format_balance_quarter_unit() {
        assert_eq!(format_balance(BALANCE_SCALE / 4), "0.25");
    }

    #[test]
    fn test_format_balance_small_value() {
        assert_eq!(format_balance(1), "0.00");
    }

    #[test]
    fn test_format_balance_large_value() {
        assert_eq!(format_balance(BALANCE_SCALE * 123), "123.00");
    }

    #[test]
    fn test_format_balance_truncation() {
        // 1 display unit = 10^10 stored; 10^7 stored = 0.001 display, truncated to 0.00
        let stored = BALANCE_SCALE / 10_000; // 0.0001 display unit
        assert_eq!(format_balance(stored), "0.00");
    }

    #[test]
    fn test_format_balance_two_decimals() {
        // 1_234_567_890 stored ≈ 0.123456789 display → truncated 0.12
        let stored = BALANCE_SCALE / 8; // 0.125 → 0.12
        assert_eq!(format_balance(stored), "0.12");
    }

    #[test]
    fn test_format_balance_negative_value() {
        // Large negative stored would produce --X.XX double-negative in naive implementation
        let stored = -(BALANCE_SCALE * 12 + BALANCE_SCALE / 8); // -12.125 display
        assert_eq!(format_balance(stored), "-12.12");
    }

    #[test]
    fn test_format_balance_negative_small() {
        let stored = -(BALANCE_SCALE / 2); // -0.50 display
        assert_eq!(format_balance(stored), "-0.50");
    }

    #[test]
    fn test_parse_display_amount_basic() {
        assert_eq!(parse_display_amount("0"), Some(0));
        assert_eq!(parse_display_amount("1"), Some(BALANCE_SCALE));
        assert_eq!(
            parse_display_amount("10.5"),
            Some(BALANCE_SCALE * 10 + BALANCE_SCALE / 2)
        );
        assert_eq!(
            parse_display_amount(" 2.25 "),
            Some(BALANCE_SCALE * 2 + BALANCE_SCALE / 4)
        );
        assert_eq!(parse_display_amount(".5"), Some(BALANCE_SCALE / 2));
        assert_eq!(parse_display_amount("3."), Some(BALANCE_SCALE * 3));
    }

    #[test]
    fn test_parse_display_amount_rejects_invalid() {
        assert_eq!(parse_display_amount(""), None);
        assert_eq!(parse_display_amount("."), None);
        assert_eq!(parse_display_amount("-1"), None);
        assert_eq!(
            parse_display_amount("1e5"),
            None,
            "scientific notation rejected"
        );
        assert_eq!(parse_display_amount("1.2.3"), None);
        assert_eq!(parse_display_amount("abc"), None);
        // 11 fractional digits exceed the stored precision.
        assert_eq!(parse_display_amount("0.12345678901"), None);
        // Overflow beyond i64 stored range.
        assert_eq!(parse_display_amount("999999999999"), None);
    }

    #[test]
    fn test_parse_display_amount_exact_beyond_f64_range() {
        // 9,000,001 display units × 10^10 = 9.000001×10^16 stored — beyond
        // f64's 2^53 exact-integer range; the decimal parser stays exact.
        assert_eq!(
            parse_display_amount("9000001"),
            Some(9_000_001 * BALANCE_SCALE)
        );
        assert_eq!(
            parse_display_amount("9000001.0000000001"),
            Some(9_000_001 * BALANCE_SCALE + 1)
        );
    }

    #[test]
    fn test_parse_display_amount_round_trips_format_balance() {
        for stored in [0, BALANCE_SCALE, BALANCE_SCALE * 42 + BALANCE_SCALE / 4] {
            let display = format_balance(stored);
            assert_eq!(parse_display_amount(&display), Some(stored));
        }
    }
}
