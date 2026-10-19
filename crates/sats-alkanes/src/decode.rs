//! Reading protostones back out of a transaction, so a transaction can be
//! checked against what was meant rather than against what the builder
//! intended to write.
//!
//! The inverse of [`crate::protostone::runestone_script`], following the
//! reference decipher path @ 62511e9: `Runestone::payload` (the first
//! `OP_RETURN OP_PUSHNUM_13` output, its data pushes concatenated),
//! `Protostone::decipher` (15-byte unpacking, then `[tag, length,
//! fields…]` per stone, stopping at tag 0), and `to_fields` (Body = 0
//! consumes everything after it).
//!
//! Deliberately narrower than the reference: anything sats never writes
//! — another runestone field, a burn, a `from`, a repeated pointer — is
//! an error rather than interpreted, so a check built on this decoder
//! refuses what it doesn't fully understand.

use sats_core::bitcoin::Transaction;
use sats_core::bitcoin::opcodes::all::{OP_PUSHNUM_13, OP_RETURN};
use sats_core::bitcoin::script::{Instruction, Script};

use crate::id::AlkaneId;
use crate::protostone::{
    Edict, MESSAGE_CHUNK, Protostone, TAG_BODY, TAG_MESSAGE, TAG_PROTO_POINTER, TAG_PROTOCOL,
    TAG_REFUND,
};
use crate::varint;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("the transaction has {0} OP_RETURN outputs (expected exactly one runestone)")]
    OpReturnCount(usize),
    #[error("the OP_RETURN output is not a runestone")]
    NotRunestone,
    #[error("malformed runestone: {0}")]
    Malformed(String),
    #[error("runestone carries field {0}, which sats never writes")]
    UnexpectedField(u128),
}

fn malformed(message: impl Into<String>) -> DecodeError {
    DecodeError::Malformed(message.into())
}

/// The single OP_RETURN output of `tx` and the protostones it carries.
/// A transaction with no OP_RETURN, or more than one, is refused: the
/// indexer reads the first runestone it finds, and an extra OP_RETURN is
/// never part of a transfer sats builds.
pub fn transaction_protostones(tx: &Transaction) -> Result<(u32, Vec<Protostone>), DecodeError> {
    let op_returns: Vec<usize> = tx
        .output
        .iter()
        .enumerate()
        .filter(|(_, out)| out.script_pubkey.is_op_return())
        .map(|(i, _)| i)
        .collect();
    let [vout] = op_returns.as_slice() else {
        return Err(DecodeError::OpReturnCount(op_returns.len()));
    };
    let stones = script_protostones(&tx.output[*vout].script_pubkey)?;
    Ok((*vout as u32, stones))
}

/// The protostones in one runestone scriptPubKey.
pub fn script_protostones(script: &Script) -> Result<Vec<Protostone>, DecodeError> {
    let mut instructions = script.instructions();
    if instructions.next() != Some(Ok(Instruction::Op(OP_RETURN)))
        || instructions.next() != Some(Ok(Instruction::Op(OP_PUSHNUM_13)))
    {
        return Err(DecodeError::NotRunestone);
    }
    let mut payload = Vec::new();
    for instruction in instructions {
        match instruction {
            Ok(Instruction::PushBytes(push)) => payload.extend_from_slice(push.as_bytes()),
            Ok(Instruction::Op(_)) => return Err(malformed("opcode inside the payload")),
            Err(_) => return Err(malformed("invalid script")),
        }
    }

    // Layer 3: tag/value pairs; sats writes only Protocol = 16383.
    let integers = varint::decode_list(&payload).map_err(malformed)?;
    if !integers.len().is_multiple_of(2) {
        return Err(malformed("odd number of runestone integers"));
    }
    let mut packed = Vec::new();
    for pair in integers.chunks(2) {
        if pair[0] != TAG_PROTOCOL {
            return Err(DecodeError::UnexpectedField(pair[0]));
        }
        packed.push(pair[1]);
    }

    // Layer 2: unpack 15 bytes per value, then the stone list.
    let raw = join_15(&packed)?;
    let values = varint::decode_list(&raw).map_err(malformed)?;
    let mut iter = values.into_iter();
    let mut stones = Vec::new();
    while let Some(protocol_tag) = iter.next() {
        // A zero tag ends the list: it is the packing's zero padding.
        if protocol_tag == 0 {
            if iter.any(|v| v != 0) {
                return Err(malformed("data after the protostone list"));
            }
            break;
        }
        let length = iter
            .next()
            .ok_or_else(|| malformed("protostone without a length"))?;
        let length = usize::try_from(length).map_err(|_| malformed("protostone too long"))?;
        let fields: Vec<u128> = iter.by_ref().take(length).collect();
        if fields.len() != length {
            return Err(malformed("protostone shorter than its length"));
        }
        stones.push(stone_from_fields(protocol_tag, &fields)?);
    }
    Ok(stones)
}

/// Each value contributes its low 15 bytes. sats never sets the 16th, so
/// a value that does is refused rather than truncated.
fn join_15(values: &[u128]) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::with_capacity(values.len() * MESSAGE_CHUNK);
    for value in values {
        let bytes = value.to_le_bytes();
        if bytes[MESSAGE_CHUNK..].iter().any(|b| *b != 0) {
            return Err(malformed("packed value uses its 16th byte"));
        }
        out.extend_from_slice(&bytes[..MESSAGE_CHUNK]);
    }
    Ok(out)
}

fn stone_from_fields(protocol_tag: u128, fields: &[u128]) -> Result<Protostone, DecodeError> {
    let mut stone = Protostone {
        protocol_tag,
        message: Vec::new(),
        pointer: None,
        refund_pointer: None,
        edicts: Vec::new(),
    };
    let mut message = Vec::new();
    let mut i = 0;
    while i < fields.len() {
        let tag = fields[i];
        if tag == TAG_BODY {
            stone.edicts = edicts_from(&fields[i + 1..])?;
            break;
        }
        let value = *fields
            .get(i + 1)
            .ok_or_else(|| malformed("field without a value"))?;
        match tag {
            TAG_PROTO_POINTER => set_once(&mut stone.pointer, value)?,
            TAG_REFUND => set_once(&mut stone.refund_pointer, value)?,
            TAG_MESSAGE => message.push(value),
            other => return Err(DecodeError::UnexpectedField(other)),
        }
        i += 2;
    }
    if !message.is_empty() {
        stone.message = join_15(&message)?;
    }
    Ok(stone)
}

fn set_once(slot: &mut Option<u32>, value: u128) -> Result<(), DecodeError> {
    if slot.is_some() {
        return Err(malformed("repeated pointer field"));
    }
    *slot = Some(u32::try_from(value).map_err(|_| malformed("pointer out of range"))?);
    Ok(())
}

/// Edicts are `[block delta, tx delta, amount, output]` groups, ids
/// delta-encoded against the previous edict
/// (`protostone_edicts_from_integers`).
fn edicts_from(values: &[u128]) -> Result<Vec<Edict>, DecodeError> {
    if !values.len().is_multiple_of(4) {
        return Err(malformed("edict values are not in groups of four"));
    }
    let mut previous = AlkaneId { block: 0, tx: 0 };
    let mut edicts = Vec::new();
    for group in values.chunks(4) {
        let overflow = || malformed("edict id overflows");
        let block = previous.block.checked_add(group[0]).ok_or_else(overflow)?;
        let tx = if group[0] == 0 {
            previous.tx.checked_add(group[1]).ok_or_else(overflow)?
        } else {
            group[1]
        };
        let id = AlkaneId { block, tx };
        let output = u32::try_from(group[3]).map_err(|_| malformed("edict output out of range"))?;
        edicts.push(Edict {
            id,
            amount: group[2],
            output,
        });
        previous = id;
    }
    Ok(edicts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::AlkaneCall;
    use crate::protostone::{ALKANES_PROTOCOL_TAG, runestone_script};
    use sats_core::bitcoin::script::{Builder, PushBytesBuf};
    use sats_core::bitcoin::{Amount, ScriptBuf, TxOut, absolute, transaction};

    fn edict(block: u128, tx: u128, amount: u128, output: u32) -> Edict {
        Edict {
            id: AlkaneId { block, tx },
            amount,
            output,
        }
    }

    fn stone(message: Vec<u8>, edicts: Vec<Edict>) -> Protostone {
        Protostone {
            protocol_tag: ALKANES_PROTOCOL_TAG,
            message,
            pointer: Some(2),
            refund_pointer: Some(2),
            edicts,
        }
    }

    fn roundtrip(stones: &[Protostone]) -> Vec<Protostone> {
        script_protostones(&runestone_script(stones).unwrap()).unwrap()
    }

    #[test]
    fn transfer_stone_roundtrips() {
        let original = stone(Vec::new(), vec![edict(32, 0, 100_000, 1)]);
        assert_eq!(roundtrip(std::slice::from_ref(&original)), vec![original]);
    }

    #[test]
    fn multiple_edicts_roundtrip_sorted_by_id() {
        // Encoding sorts by id; the decoded order is the sorted order.
        let original = stone(
            Vec::new(),
            vec![
                edict(32, 0, 5, 1),
                edict(2, 7, u128::MAX, 1),
                edict(2, 3, 9, 3),
            ],
        );
        let decoded = roundtrip(&[original]);
        assert_eq!(
            decoded[0].edicts,
            vec![
                edict(2, 3, 9, 3),
                edict(2, 7, u128::MAX, 1),
                edict(32, 0, 5, 1)
            ]
        );
    }

    #[test]
    fn call_stone_roundtrips_its_padded_message() {
        let message = AlkaneCall {
            target: AlkaneId { block: 2, tx: 1 },
            inputs: vec![77],
        }
        .encode_cellpack();
        let decoded = roundtrip(&[stone(message.clone(), Vec::new())]);
        // The 15-byte packing zero-pads the message (see `call.rs`).
        assert_eq!(&decoded[0].message[..message.len()], message.as_slice());
        assert!(decoded[0].message[message.len()..].iter().all(|b| *b == 0));
    }

    #[test]
    fn the_frozen_call_vector_decodes() {
        // `protostone.rs`'s reference vector: call 2:1 with [77], pointer 0.
        let script = ScriptBuf::from_bytes(hex::decode("6a5d0cff7f8188ec8290caa0c1b405").unwrap());
        let decoded = script_protostones(&script).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].pointer, Some(0));
        assert_eq!(decoded[0].refund_pointer, None);
        assert!(decoded[0].edicts.is_empty());
    }

    #[test]
    fn foreign_fields_and_scripts_are_refused() {
        // A runestone Pointer (22) field: sats never writes one.
        let mut payload = Vec::new();
        varint::encode_to_vec(22, &mut payload);
        varint::encode_to_vec(1, &mut payload);
        let script = Builder::new()
            .push_opcode(OP_RETURN)
            .push_opcode(OP_PUSHNUM_13)
            .push_slice(PushBytesBuf::try_from(payload).unwrap())
            .into_script();
        assert_eq!(
            script_protostones(&script),
            Err(DecodeError::UnexpectedField(22))
        );

        // An OP_RETURN without the runestone magic.
        let plain = Builder::new().push_opcode(OP_RETURN).into_script();
        assert_eq!(script_protostones(&plain), Err(DecodeError::NotRunestone));
    }

    #[test]
    fn exactly_one_op_return_is_required() {
        let runestone = runestone_script(&[stone(Vec::new(), vec![edict(2, 0, 1, 1)])]).unwrap();
        let out = |script: ScriptBuf| TxOut {
            value: Amount::ZERO,
            script_pubkey: script,
        };
        let mut tx = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: Vec::new(),
            output: vec![out(ScriptBuf::new())],
        };
        assert_eq!(
            transaction_protostones(&tx),
            Err(DecodeError::OpReturnCount(0))
        );
        tx.output.push(out(runestone.clone()));
        assert_eq!(transaction_protostones(&tx).unwrap().0, 1);
        tx.output.push(out(runestone));
        assert_eq!(
            transaction_protostones(&tx),
            Err(DecodeError::OpReturnCount(2))
        );
    }
}
