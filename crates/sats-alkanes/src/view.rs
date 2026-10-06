//! Requests for the indexer's view functions, called through
//! `metashrew_view(name, "0x" + hex(request), "latest")`.
//!
//! A view request is a protobuf message (alkanes-rs
//! `crates/alkanes-support/proto/alkanes.proto`): `uint128 { uint64 lo = 1;
//! uint64 hi = 2; }`, `AlkaneId { uint128 block = 1; uint128 tx = 2; }`,
//! and `BytecodeRequest { AlkaneId id = 1; }`. Proto3 omits zero scalars,
//! but every message field is written, even when empty: the indexer
//! unwraps them.

use crate::varint;

/// Wire type 0: a varint scalar.
fn put_varint(field: u32, value: u64, out: &mut Vec<u8>) {
    if value != 0 {
        varint::encode_to_vec(u128::from(field << 3), out);
        varint::encode_to_vec(u128::from(value), out);
    }
}

/// Wire type 2: a length-delimited nested message.
fn put_message(field: u32, body: &[u8], out: &mut Vec<u8>) {
    varint::encode_to_vec(u128::from(field << 3 | 2), out);
    varint::encode_to_vec(body.len() as u128, out);
    out.extend_from_slice(body);
}

fn uint128(value: u128) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(1, value as u64, &mut out);
    put_varint(2, (value >> 64) as u64, &mut out);
    out
}

fn alkane_id(block: u128, tx: u128) -> Vec<u8> {
    let mut out = Vec::new();
    put_message(1, &uint128(block), &mut out);
    put_message(2, &uint128(tx), &mut out);
    out
}

/// The `getbytecode` view's request for one alkane id.
pub fn bytecode_request(block: u128, tx: u128) -> Vec<u8> {
    let mut out = Vec::new();
    put_message(1, &alkane_id(block, tx), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frozen against a live signet indexer: this request returned the
    /// bytecode of 2:0.
    #[test]
    fn bytecode_request_matches_the_indexer() {
        assert_eq!(hex::encode(bytecode_request(2, 0)), "0a060a0208021200");
    }

    #[test]
    fn multi_byte_ids_and_the_high_half_are_encoded() {
        // 32 → 08 20; 300 → 08 ac 02 (two varint bytes).
        assert_eq!(
            hex::encode(bytecode_request(32, 300)),
            "0a090a020820120308ac02"
        );
        // 2^64 + 1 → lo 1 (08 01), hi 1 (10 01); tx 0 stays present, empty.
        assert_eq!(
            hex::encode(bytecode_request(u128::from(u64::MAX) + 2, 0)),
            "0a080a04080110011200"
        );
    }
}
