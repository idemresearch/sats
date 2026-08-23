//! Unsigned LEB128, exactly as the runes/alkanes stack encodes it.
//!
//! Reference: `crates/ordinals/src/varint.rs` in alkanes-rs @ 62511e9 —
//! 7-bit groups, least significant first, continuation bit 0x80 on every
//! byte but the last.

pub fn encode_to_vec(mut n: u128, out: &mut Vec<u8>) {
    while n >> 7 > 0 {
        out.push((n & 0x7f) as u8 | 0x80);
        n >>= 7;
    }
    out.push((n & 0x7f) as u8);
}

pub fn encode(n: u128) -> Vec<u8> {
    let mut out = Vec::new();
    encode_to_vec(n, &mut out);
    out
}

/// A list of u128s is the plain concatenation of their varints
/// (`encode_varint_list` in protorune-support's utils.rs).
pub fn encode_list(values: &[u128]) -> Vec<u8> {
    let mut out = Vec::new();
    for value in values {
        encode_to_vec(*value, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_reference_vectors() {
        // Computed by hand from the reference algorithm and cross-checked
        // against an independent implementation.
        assert_eq!(encode(0), [0x00]);
        assert_eq!(encode(127), [0x7f]);
        assert_eq!(encode(128), [0x80, 0x01]);
        assert_eq!(encode(16_383), [0xff, 0x7f]);
        assert_eq!(encode(16_384), [0x80, 0x80, 0x01]);
    }

    #[test]
    fn u128_max_is_nineteen_bytes() {
        let bytes = encode(u128::MAX);
        assert_eq!(bytes.len(), 19);
        assert!(bytes[..18].iter().all(|b| b & 0x80 != 0));
        assert_eq!(bytes[18] & 0x80, 0);
    }

    #[test]
    fn list_is_plain_concatenation() {
        assert_eq!(encode_list(&[2, 1, 77]), [0x02, 0x01, 0x4d]);
    }
}
