//! Checking the transaction for a generic call. This verifies its encoding
//! and Bitcoin disposition, not the contract's future effects.

use crate::call::AlkaneCall;
use crate::decode::{DecodeError, transaction_protostones};
use crate::protostone::{ALKANES_PROTOCOL_TAG, MESSAGE_CHUNK};
use sats_core::bitcoin::Transaction;

#[derive(Debug, thiserror::Error)]
pub enum ExecutionCheckError {
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error("refusing this execution: {0}")]
    Refused(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteSummary {
    pub postage_sat: u64,
    pub fee_sat: u64,
}

/// Decode the call and enforce the builder's supported shape: one call,
/// pointer/refund to output 1, no edicts, zero-value runestone, and every
/// spendable output returning to the wallet. Input values come from trusted
/// wallet UTXOs. Contract effects remain advisory.
pub fn summarize(
    tx: &Transaction,
    input_sat: &[u64],
    call: &AlkaneCall,
    postage_sat: u64,
    wallet_owned: impl Fn(u32) -> bool,
) -> Result<ExecuteSummary, ExecutionCheckError> {
    use ExecutionCheckError::Refused;
    let (vout, stones) = transaction_protostones(tx)?;
    let [stone] = stones.as_slice() else {
        return Err(Refused("expected exactly one call"));
    };
    let mut message = call.encode_cellpack();
    message.resize(message.len().div_ceil(MESSAGE_CHUNK) * MESSAGE_CHUNK, 0);
    if vout != 0
        || stone.protocol_tag != ALKANES_PROTOCOL_TAG
        || stone.message != message
        || stone.pointer != Some(1)
        || stone.refund_pointer != Some(1)
        || !stone.edicts.is_empty()
    {
        return Err(Refused("transaction does not match the requested call"));
    }
    if tx.output[0].value.to_sat() != 0 {
        return Err(Refused("runestone carries Bitcoin"));
    }
    if tx.output.get(1).map(|out| out.value.to_sat()) != Some(postage_sat) {
        return Err(Refused("postage does not match the request"));
    }
    if (1..tx.output.len()).any(|i| !wallet_owned(i as u32)) {
        return Err(Refused("a spendable output is not owned by the wallet"));
    }
    if tx.input.is_empty() || input_sat.len() != tx.input.len() {
        return Err(Refused("missing input values"));
    }
    let inputs = input_sat
        .iter()
        .try_fold(0u64, |sum, value| sum.checked_add(*value))
        .ok_or(Refused("input values overflow"))?;
    let outputs = tx
        .output
        .iter()
        .try_fold(0u64, |sum, out| sum.checked_add(out.value.to_sat()))
        .ok_or(Refused("output values overflow"))?;
    let fee_sat = inputs
        .checked_sub(outputs)
        .ok_or(Refused("outputs exceed inputs"))?;
    Ok(ExecuteSummary {
        postage_sat,
        fee_sat,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::AlkaneId;
    use crate::protostone::{Edict, Protostone, runestone_script};
    use sats_core::bitcoin::{Amount, ScriptBuf, TxIn, TxOut, absolute, transaction};

    fn call() -> AlkaneCall {
        AlkaneCall {
            target: AlkaneId { block: 2, tx: 1 },
            inputs: vec![77],
        }
    }
    fn stone() -> Protostone {
        Protostone {
            protocol_tag: ALKANES_PROTOCOL_TAG,
            message: call().encode_cellpack(),
            pointer: Some(1),
            refund_pointer: Some(1),
            edicts: vec![],
        }
    }
    fn tx(stone: Protostone) -> Transaction {
        Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![
                TxOut {
                    value: Amount::ZERO,
                    script_pubkey: runestone_script(&[stone]).unwrap(),
                },
                TxOut {
                    value: Amount::from_sat(546),
                    script_pubkey: ScriptBuf::new(),
                },
                TxOut {
                    value: Amount::from_sat(9_000),
                    script_pubkey: ScriptBuf::new(),
                },
            ],
        }
    }
    #[test]
    fn verifies_call_postage_and_real_fee_from_the_transaction() {
        assert_eq!(
            summarize(&tx(stone()), &[10_000], &call(), 546, |_| true).unwrap(),
            ExecuteSummary {
                postage_sat: 546,
                fee_sat: 454
            }
        );
    }
    #[test]
    fn refuses_different_calls_pointers_edicts_and_bitcoin_destinations() {
        let mut changed_call = stone();
        changed_call.message = AlkaneCall {
            target: AlkaneId { block: 32, tx: 0 },
            inputs: vec![77],
        }
        .encode_cellpack();
        let mut changed_pointer = stone();
        changed_pointer.pointer = Some(2);
        let mut changed_refund = stone();
        changed_refund.refund_pointer = Some(2);
        let mut edict = stone();
        edict.edicts.push(Edict {
            id: AlkaneId { block: 2, tx: 1 },
            amount: 1,
            output: 1,
        });
        for stone in [changed_call, changed_pointer, changed_refund, edict] {
            assert!(summarize(&tx(stone), &[10_000], &call(), 546, |_| true).is_err());
        }
        assert!(summarize(&tx(stone()), &[10_000], &call(), 546, |i| i != 2).is_err());
        let mut paid_runestone = tx(stone());
        paid_runestone.output[0].value = Amount::from_sat(1);
        assert!(summarize(&paid_runestone, &[10_000], &call(), 546, |_| true).is_err());
        assert!(summarize(&tx(stone()), &[10_000], &call(), 547, |_| true).is_err());
    }
    #[test]
    fn refuses_missing_values_and_underfunded_transactions() {
        assert!(summarize(&tx(stone()), &[], &call(), 546, |_| true).is_err());
        assert!(summarize(&tx(stone()), &[1], &call(), 546, |_| true).is_err());
    }
}
