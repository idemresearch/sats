//! Contract inspection primitives: bytecode identity.

use sats_core::bitcoin::hashes::{Hash, sha256};

/// Lowercase-hex sha256 of the contract bytecode: the stable identity a
/// human can compare against an audited build.
pub fn code_hash(bytecode: &[u8]) -> String {
    sha256::Hash::hash(bytecode).to_string()
}

/// Decode hex bytecode as endpoints return it, tolerating a 0x prefix.
pub fn decode_bytecode_hex(hex_str: &str) -> Result<Vec<u8>, String> {
    let stripped = hex_str
        .strip_prefix("0x")
        .or_else(|| hex_str.strip_prefix("0X"))
        .unwrap_or(hex_str);
    hex::decode(stripped.trim()).map_err(|e| format!("invalid bytecode hex: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_hash_is_plain_sha256_hex() {
        // sha256("") — the canonical empty-input vector.
        assert_eq!(
            code_hash(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            code_hash(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn bytecode_hex_tolerates_prefix_and_whitespace() {
        assert_eq!(decode_bytecode_hex("0x0061736d").unwrap(), b"\0asm");
        assert_eq!(decode_bytecode_hex("0061736d\n").unwrap(), b"\0asm");
        assert!(decode_bytecode_hex("zz").is_err());
    }
}
