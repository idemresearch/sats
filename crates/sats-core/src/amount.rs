//! Amount parsing: plain integer sats, or `k`/`m` shorthand (`10k`,
//! `1.5m`). Shorthand must resolve to whole sats.

pub fn parse(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty amount".into());
    }
    let (number, multiplier) = match s.chars().last().unwrap().to_ascii_lowercase() {
        'k' => (&s[..s.len() - 1], 1_000u64),
        'm' => (&s[..s.len() - 1], 1_000_000u64),
        _ => (s, 1),
    };
    if number.is_empty() {
        return Err(format!("invalid amount {s:?}"));
    }

    if multiplier == 1 {
        return number
            .parse::<u64>()
            .map_err(|_| format!("invalid amount {s:?} (sats, or shorthand like 10k, 1.5m)"));
    }

    // Shorthand may carry a decimal part, but must land on whole sats.
    let (int_part, frac_part) = match number.split_once('.') {
        Some((i, f)) => (i, f),
        None => (number, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(format!("invalid amount {s:?}"));
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return Err(format!(
            "invalid amount {s:?} (sats, or shorthand like 10k, 1.5m)"
        ));
    }
    let max_frac_digits = if multiplier == 1_000 { 3 } else { 6 };
    if frac_part.len() > max_frac_digits && frac_part[max_frac_digits..].chars().any(|c| c != '0') {
        return Err(format!("{s:?} is not a whole number of sats"));
    }

    let int_val = if int_part.is_empty() {
        0
    } else {
        int_part
            .parse::<u64>()
            .map_err(|_| format!("amount {s:?} is too large"))?
    };
    let int_sats = int_val
        .checked_mul(multiplier)
        .ok_or_else(|| format!("amount {s:?} is too large"))?;
    let frac_digits: &str = &frac_part[..frac_part.len().min(max_frac_digits)];
    let frac_sats = if frac_digits.is_empty() {
        0
    } else {
        frac_digits.parse::<u64>().unwrap_or(0)
            * 10u64.pow((max_frac_digits - frac_digits.len()) as u32)
    };
    int_sats
        .checked_add(frac_sats)
        .ok_or_else(|| format!("amount {s:?} is too large"))
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn plain_integers() {
        assert_eq!(parse("0").unwrap(), 0);
        assert_eq!(parse("25000").unwrap(), 25_000);
        assert_eq!(parse(" 42 ").unwrap(), 42);
    }

    #[test]
    fn k_and_m_shorthand() {
        assert_eq!(parse("10k").unwrap(), 10_000);
        assert_eq!(parse("50K").unwrap(), 50_000);
        assert_eq!(parse("1m").unwrap(), 1_000_000);
        assert_eq!(parse("2M").unwrap(), 2_000_000);
    }

    #[test]
    fn decimal_shorthand_landing_on_whole_sats() {
        assert_eq!(parse("1.5k").unwrap(), 1_500);
        assert_eq!(parse("0.5k").unwrap(), 500);
        assert_eq!(parse(".5k").unwrap(), 500);
        assert_eq!(parse("1.234k").unwrap(), 1_234);
        assert_eq!(parse("1.5m").unwrap(), 1_500_000);
        assert_eq!(parse("0.000001m").unwrap(), 1);
        assert_eq!(parse("1.5000k").unwrap(), 1_500);
    }

    #[test]
    fn sub_sat_amounts_rejected() {
        assert!(parse("1.2345k").unwrap_err().contains("whole number"));
        assert!(parse("0.0000001m").unwrap_err().contains("whole number"));
    }

    #[test]
    fn garbage_rejected() {
        for bad in [
            "", "k", ".k", "1.5", "1..5k", "1,000", "-5k", "5kk", "abc", "1.5b",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn plain_decimals_rejected() {
        // Decimals only make sense with a multiplier suffix.
        assert!(parse("1.5").is_err());
    }

    #[test]
    fn overflow_rejected() {
        assert!(parse("999999999999999999m").is_err());
        assert_eq!(parse("18446744073709551615").unwrap(), u64::MAX);
    }
}
