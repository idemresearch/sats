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

/// Decode one varint from the front of `buffer`: the value and the bytes
/// it used. As the reference: at most 19 bytes, and the 19th may carry
/// only the two bits a u128 has left.
pub fn decode(buffer: &[u8]) -> Result<(u128, usize), &'static str> {
    let mut n = 0u128;
    for (i, &byte) in buffer.iter().enumerate() {
        if i > 18 {
            return Err("varint longer than 19 bytes");
        }
        let value = u128::from(byte & 0x7f);
        if i == 18 && value & 0x7c != 0 {
            return Err("varint overflows u128");
        }
        n |= value << (7 * i);
        if byte & 0x80 == 0 {
            return Ok((n, i + 1));
        }
    }
    Err("unterminated varint")
}

/// Decode a buffer that is nothing but concatenated varints.
pub fn decode_list(mut buffer: &[u8]) -> Result<Vec<u128>, &'static str> {
    let mut values = Vec::new();
    while !buffer.is_empty() {
        let (value, used) = decode(buffer)?;
        values.push(value);
        buffer = &buffer[used..];
    }
    Ok(values)
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
    fn decode_inverts_encode() {
        for n in [
            0,
            1,
            127,
            128,
            16_383,
            16_384,
            u128::from(u64::MAX),
            u128::MAX,
        ] {
            assert_eq!(decode(&encode(n)), Ok((n, encode(n).len())));
        }
        let list = [2, 1, 77, u128::MAX, 0];
        assert_eq!(decode_list(&encode_list(&list)), Ok(list.to_vec()));
    }

    #[test]
    fn decode_refuses_malformed_input() {
        assert!(decode(&[0x80]).is_err(), "unterminated");
        assert!(decode(&[0xff; 20]).is_err(), "overlong");
        let mut overflow = vec![0xff; 18];
        overflow.push(0x7f);
        assert!(decode(&overflow).is_err(), "overflow");
    }

    #[test]
    fn list_is_plain_concatenation() {
        assert_eq!(encode_list(&[2, 1, 77]), [0x02, 0x01, 0x4d]);
    }
}
