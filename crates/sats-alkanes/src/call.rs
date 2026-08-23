//! A contract call: the cellpack.
//!
//! Reference: `crates/alkanes-support/src/cellpack.rs` in alkanes-rs @
//! 62511e9 — a cellpack is `[target.block, target.tx, inputs...]` as one
//! concatenated LEB128 list; the first input is conventionally the
//! opcode.

use serde::{Deserialize, Serialize};

use crate::id::AlkaneId;
use crate::varint;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlkaneCall {
    pub target: AlkaneId,
    /// Calldata words; the first is conventionally the opcode.
    pub inputs: Vec<u128>,
}

impl AlkaneCall {
    pub fn to_values(&self) -> Vec<u128> {
        let mut values = Vec::with_capacity(2 + self.inputs.len());
        values.push(self.target.block);
        values.push(self.target.tx);
        values.extend(&self.inputs);
        values
    }

    /// The protostone message bytes. Note the decode-side padding rule:
    /// the 15-byte u128 packing zero-pads the message, and every trailing
    /// 0x00 decodes as an extra `0` input — contracts must not depend on
    /// the exact input count.
    pub fn encode_cellpack(&self) -> Vec<u8> {
        varint::encode_list(&self.to_values())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cellpack_matches_the_reference_vector() {
        // {block: 2, tx: 1}, inputs [77] → 02 01 4d, per the reference
        // encoding (verified against alkanes-rs @ 62511e9).
        let call = AlkaneCall {
            target: AlkaneId { block: 2, tx: 1 },
            inputs: vec![77],
        };
        assert_eq!(call.encode_cellpack(), [0x02, 0x01, 0x4d]);
    }

    #[test]
    fn large_words_encode_at_full_width() {
        let call = AlkaneCall {
            target: AlkaneId { block: 2, tx: 1 },
            inputs: vec![77, 1u128 << 64],
        };
        assert_eq!(
            call.encode_cellpack(),
            [
                0x02, 0x01, 0x4d, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02
            ]
        );
    }

    #[test]
    fn empty_inputs_are_target_only() {
        let call = AlkaneCall {
            target: AlkaneId { block: 4, tx: 0 },
            inputs: vec![],
        };
        assert_eq!(call.encode_cellpack(), [0x04, 0x00]);
    }
}
