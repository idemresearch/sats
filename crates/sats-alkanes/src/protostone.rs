//! The runestone OP_RETURN envelope carrying protostones.
//!
//! Three encoding layers, each from the alkanes-rs reference @ 62511e9:
//!
//! 1. A protostone's fields become tag/value u128 pairs
//!    (`crates/protorune-support/src/protostone.rs::to_integers`):
//!    ProtoPointer = 91, Refund = 93, then one Message = 81 pair per
//!    15-byte chunk of the message, packed little-endian into a u128
//!    (`byte_utils.rs::snap_to_15_bytes` — 15 bytes so the packed
//!    varint never reaches a cenotaph-length 19 bytes).
//! 2. The protostone list becomes `[protocol_tag, field_count, fields…]`
//!    per stone, LEB128-encoded and re-packed 15 bytes per u128
//!    (`crates/protorune/src/protostone.rs::encipher`).
//! 3. The runestone writes `varint(16383) varint(value)` for every packed
//!    u128 — Tag::Protocol = 16383, repeated before each value
//!    (`crates/ordinals/src/runestone.rs` + `runestone/tag.rs`) — after
//!    `OP_RETURN OP_PUSHNUM_13`, chunked into data pushes of at most 520
//!    bytes (`bitcoin::constants::MAX_SCRIPT_ELEMENT_SIZE`).

use sats_core::bitcoin::opcodes::all::{OP_PUSHNUM_13, OP_RETURN};
use sats_core::bitcoin::script::{Builder, PushBytesBuf, ScriptBuf};

use crate::varint;

/// The alkanes protocol tag (`src/message.rs::protocol_tag()` returns 1).
pub const ALKANES_PROTOCOL_TAG: u128 = 1;

const TAG_PROTOCOL: u128 = 16_383;
const TAG_MESSAGE: u128 = 81;
const TAG_PROTO_POINTER: u128 = 91;
const TAG_REFUND: u128 = 93;
const MESSAGE_CHUNK: usize = 15;
const MAX_PUSH: usize = 520;

/// Our own conservative ceiling on the whole scriptPubKey; the reference
/// enforces only the 520-byte push chunking.
const MAX_SCRIPT_BYTES: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protostone {
    pub protocol_tag: u128,
    /// The protostone message — for alkanes, an encoded cellpack.
    pub message: Vec<u8>,
    /// Output index receiving the protostone's assets.
    pub pointer: Option<u32>,
    /// Output index refunded on failure.
    pub refund_pointer: Option<u32>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EncodeError {
    #[error("runestone script would be {bytes} bytes (max {max})")]
    TooLarge { bytes: usize, max: usize },
}

/// 15 bytes per u128, little-endian, zero-padded — the packing shared by
/// the message layer and the outer protostone list.
fn split_bytes(data: &[u8]) -> Vec<u128> {
    data.chunks(MESSAGE_CHUNK)
        .map(|chunk| {
            let mut bytes = [0u8; 16];
            bytes[..chunk.len()].copy_from_slice(chunk);
            u128::from_le_bytes(bytes)
        })
        .collect()
}

impl Protostone {
    /// Field pairs in the reference's emission order.
    fn to_integers(&self) -> Vec<u128> {
        let mut values = Vec::new();
        if let Some(pointer) = self.pointer {
            values.push(TAG_PROTO_POINTER);
            values.push(u128::from(pointer));
        }
        if let Some(refund) = self.refund_pointer {
            values.push(TAG_REFUND);
            values.push(u128::from(refund));
        }
        for chunk in split_bytes(&self.message) {
            values.push(TAG_MESSAGE);
            values.push(chunk);
        }
        values
    }
}

/// The full OP_RETURN scriptPubKey embedding the given protostones.
pub fn runestone_script(stones: &[Protostone]) -> Result<ScriptBuf, EncodeError> {
    let mut values = Vec::new();
    for stone in stones {
        let fields = stone.to_integers();
        values.push(stone.protocol_tag);
        values.push(fields.len() as u128);
        values.extend(fields);
    }
    let packed = split_bytes(&varint::encode_list(&values));
    let mut payload = Vec::new();
    for value in &packed {
        varint::encode_to_vec(TAG_PROTOCOL, &mut payload);
        varint::encode_to_vec(*value, &mut payload);
    }

    let mut builder = Builder::new()
        .push_opcode(OP_RETURN)
        .push_opcode(OP_PUSHNUM_13);
    for chunk in payload.chunks(MAX_PUSH) {
        let push = PushBytesBuf::try_from(chunk.to_vec()).expect("chunks are at most 520 bytes");
        builder = builder.push_slice(push);
    }
    let script = builder.into_script();
    if script.len() > MAX_SCRIPT_BYTES {
        return Err(EncodeError::TooLarge {
            bytes: script.len(),
            max: MAX_SCRIPT_BYTES,
        });
    }
    Ok(script)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::AlkaneCall;
    use crate::id::AlkaneId;

    fn call_stone(
        target: AlkaneId,
        inputs: Vec<u128>,
        pointer: Option<u32>,
        refund: Option<u32>,
    ) -> Protostone {
        Protostone {
            protocol_tag: ALKANES_PROTOCOL_TAG,
            message: AlkaneCall { target, inputs }.encode_cellpack(),
            pointer,
            refund_pointer: refund,
        }
    }

    #[test]
    fn script_matches_the_frozen_reference_vector() {
        // {2:1} inputs [77], pointer 0: derived by hand from the reference
        // rules and cross-checked against an independent implementation.
        // to_integers = [91, 0, 81, 5046530]; outer = [1, 4, 91, 0, 81,
        // 5046530]; one packed u128; payload = ff7f + leb(u128).
        let script = runestone_script(&[call_stone(
            AlkaneId { block: 2, tx: 1 },
            vec![77],
            Some(0),
            None,
        )])
        .unwrap();
        assert_eq!(
            script.to_bytes(),
            hex::decode("6a5d0cff7f8188ec8290caa0c1b405").unwrap()
        );
    }

    #[test]
    fn pointer_and_refund_both_encode() {
        // {2:0} inputs [77] (the classic mint shape), pointer 1, refund 1.
        let script = runestone_script(&[call_stone(
            AlkaneId { block: 2, tx: 0 },
            vec![77],
            Some(1),
            Some(1),
        )])
        .unwrap();
        assert_eq!(
            script.to_bytes(),
            hex::decode("6a5d0eff7f818cec8ad0abc0a88281d215").unwrap()
        );
    }

    #[test]
    fn long_payloads_pack_into_multiple_tagged_values() {
        // A 13-byte cellpack pushes the outer list past one 15-byte pack:
        // two u128s, each behind its own tag-16383 varint pair.
        let script = runestone_script(&[call_stone(
            AlkaneId { block: 2, tx: 1 },
            vec![77, 1u128 << 64],
            Some(1),
            Some(1),
        )])
        .unwrap();
        assert_eq!(
            script.to_bytes(),
            hex::decode("6a5d1dff7f818cec8ad0abc0a88285d2958891a4d0c001ff7f80838aa4889114")
                .unwrap()
        );
    }

    #[test]
    fn script_always_starts_op_return_op_13() {
        let script =
            runestone_script(&[call_stone(AlkaneId { block: 2, tx: 1 }, vec![], None, None)])
                .unwrap();
        let bytes = script.to_bytes();
        assert_eq!(bytes[0], 0x6a, "OP_RETURN");
        assert_eq!(bytes[1], 0x5d, "OP_PUSHNUM_13");
    }

    #[test]
    fn oversized_payload_is_a_typed_error() {
        let stone = Protostone {
            protocol_tag: ALKANES_PROTOCOL_TAG,
            message: vec![0xff; 12_000],
            pointer: Some(1),
            refund_pointer: Some(1),
        };
        assert!(matches!(
            runestone_script(&[stone]),
            Err(EncodeError::TooLarge { .. })
        ));
    }

    #[test]
    fn empty_message_still_frames_the_stone() {
        // [1, 0] → leb 01 00 → one packed u128 = 1; payload ff7f 01.
        let stone = Protostone {
            protocol_tag: ALKANES_PROTOCOL_TAG,
            message: Vec::new(),
            pointer: None,
            refund_pointer: None,
        };
        let script = runestone_script(&[stone]).unwrap();
        assert_eq!(script.to_bytes(), hex::decode("6a5d03ff7f01").unwrap());
    }
}
