//! Number formatting shared by every frontend.

/// Format an integer amount of sats with thousands separators: `25412` → `"25,412"`.
pub fn format_sats(sats: u64) -> String {
    let digits = sats.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separates_thousands() {
        assert_eq!(format_sats(0), "0");
        assert_eq!(format_sats(1), "1");
        assert_eq!(format_sats(999), "999");
        assert_eq!(format_sats(1_000), "1,000");
        assert_eq!(format_sats(25_412), "25,412");
        assert_eq!(format_sats(100_000_000), "100,000,000");
        assert_eq!(format_sats(u64::MAX), "18,446,744,073,709,551,615");
    }
}
