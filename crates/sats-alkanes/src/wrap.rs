//! Narrow frBTC wrapping: context encoding, signer response, composition,
//! and independent transaction checking. No environment effects.
//!
//! Reference: alkanes-rs 18764c8f86985099acf7e19607a0042b5641023b,
//! alkanes-support/proto/alkanes.proto and alkanes-cli-common/subfrost.rs.

use sats_core::bdk_wallet::{TxOrdering, Wallet};
use sats_core::bitcoin::consensus::serialize;
use sats_core::bitcoin::secp256k1::{Secp256k1, XOnlyPublicKey};
use sats_core::bitcoin::{Address, Amount, FeeRate, Network, OutPoint, ScriptBuf, Transaction};
use sats_core::plan::PreparedSpend;

use crate::build::{BuildError, DEFAULT_POSTAGE_SAT};
use crate::call::AlkaneCall;
use crate::decode::transaction_protostones;
use crate::id::AlkaneId;
use crate::protostone::{ALKANES_PROTOCOL_TAG, MESSAGE_CHUNK, Protostone, runestone_script};
use crate::view::{Fields, Value, put_message, put_varint};

pub const FRBTC: AlkaneId = AlkaneId { block: 32, tx: 0 };
pub const WRAP_OPCODE: u128 = 77;
pub const SIGNER_OPCODE: u128 = 103;

pub fn call(opcode: u128) -> AlkaneCall {
    AlkaneCall {
        target: FRBTC,
        inputs: vec![opcode],
    }
}

/// A protobuf MessageContextParcel for the `simulate` indexer view.
/// Wrap simulations include the exact unsigned transaction and its pointers.
pub fn context_request(
    call: &AlkaneCall,
    tx: Option<&Transaction>,
    height: u64,
    vout: u32,
    pointer: u32,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    if let Some(tx) = tx {
        put_message(2, &serialize(tx), &mut bytes);
    }
    put_varint(4, height, &mut bytes);
    put_message(5, &call.encode_cellpack(), &mut bytes);
    put_varint(7, u64::from(vout), &mut bytes);
    put_varint(8, u64::from(pointer), &mut bytes);
    put_varint(9, u64::from(pointer), &mut bytes);
    bytes
}

/// Strictly decode SimulateResponse. An execution must be present and
/// every error field must be empty. Do not expose arbitrary provider errors.
fn execution(bytes: &[u8]) -> Result<&[u8], String> {
    let mut execution = None;
    for field in Fields::new(bytes) {
        match field? {
            (1, Value::Bytes(value)) if execution.is_none() => execution = Some(value),
            (1, _) => return Err("duplicate or malformed simulation execution".into()),
            (3, Value::Bytes([])) => {}
            (3, _) => return Err("simulation reported an error".into()),
            _ => {}
        }
    }
    execution.ok_or_else(|| "simulation did not report a recognized execution".into())
}

pub fn signer_address(bytes: &[u8], network: Network) -> Result<Address, String> {
    sats_core::network::ensure_test_network(network).map_err(|e| e.to_string())?;
    let mut data = None;
    for field in Fields::new(execution(bytes)?) {
        match field? {
            (3, Value::Bytes(value)) if data.is_none() => data = Some(value),
            (3, _) => return Err("duplicate or malformed signer data".into()),
            _ => {}
        }
    }
    let data = data.ok_or("signer simulation has no data")?;
    let address = match data {
        // Current contracts return the full P2TR script. Its output key
        // is already tweaked: never apply another taproot tweak to it.
        [0x51, 0x20, key @ ..] if key.len() == 32 => {
            XOnlyPublicKey::from_slice(key).map_err(|_| "invalid signer output key")?;
            Address::from_script(&ScriptBuf::from_bytes(data.to_vec()), network)
                .map_err(|_| "invalid signer payment script")?
        }
        key if key.len() == 32 => {
            let key = XOnlyPublicKey::from_slice(key).map_err(|_| "invalid signer internal key")?;
            Address::p2tr(&Secp256k1::verification_only(), key, None, network)
        }
        _ => {
            return Err("unrecognized signer data: expected an internal key or P2TR script".into());
        }
    };
    Ok(address)
}

fn uint128(bytes: &[u8]) -> Result<u128, String> {
    let mut halves = [None, None];
    for field in Fields::new(bytes) {
        match field? {
            (number @ 1..=2, Value::Varint(value)) => {
                let slot = &mut halves[number as usize - 1];
                if slot.is_some() {
                    return Err("duplicate uint128 half".into());
                }
                *slot = Some(u64::try_from(value).map_err(|_| "uint128 half overflows")?);
            }
            _ => return Err("unrecognized uint128 encoding".into()),
        }
    }
    Ok(u128::from(halves[0].unwrap_or(0)) | (u128::from(halves[1].unwrap_or(0)) << 64))
}

/// Require one positive frBTC prediction, bounded by the BTC payment.
/// This verifies the recognized response shape, not future contract effects.
pub fn predicted_frbtc(bytes: &[u8], amount_sat: u64) -> Result<u128, String> {
    let mut prediction = None;
    for field in Fields::new(execution(bytes)?) {
        if let (1, Value::Bytes(transfer)) = field? {
            if prediction.is_some() {
                return Err("wrap predicted unexpected extra assets".into());
            }
            let mut id = None;
            let mut value = None;
            for field in Fields::new(transfer) {
                match field? {
                    (1, Value::Bytes(raw)) if id.is_none() => {
                        let mut parts = [None, None];
                        for field in Fields::new(raw) {
                            match field? {
                                (n @ 1..=2, Value::Bytes(v)) if parts[n as usize - 1].is_none() => {
                                    parts[n as usize - 1] = Some(uint128(v)?)
                                }
                                _ => return Err("invalid predicted asset id".into()),
                            }
                        }
                        id = Some(AlkaneId {
                            block: parts[0].ok_or("missing block")?,
                            tx: parts[1].ok_or("missing tx")?,
                        });
                    }
                    (2, Value::Bytes(raw)) if value.is_none() => value = Some(uint128(raw)?),
                    _ => return Err("invalid predicted transfer".into()),
                }
            }
            let value = value.ok_or("missing predicted amount")?;
            if id != Some(FRBTC) || value == 0 || value > u128::from(amount_sat) {
                return Err("wrap did not predict a valid frBTC amount".into());
            }
            prediction = Some(value);
        }
    }
    prediction.ok_or_else(|| "wrap simulation predicted no frBTC".into())
}

#[allow(clippy::too_many_arguments)]
pub fn build_plan(
    wallet: &mut Wallet,
    signer: &Address,
    recipient: &Address,
    amount_sat: u64,
    fee_rate: FeeRate,
    excluded: &[OutPoint],
    network: &str,
    now: u64,
) -> Result<PreparedSpend, BuildError> {
    let stone = Protostone {
        protocol_tag: ALKANES_PROTOCOL_TAG,
        message: call(WRAP_OPCODE).encode_cellpack(),
        pointer: Some(1),
        refund_pointer: Some(1),
        edicts: vec![],
    };
    let script = runestone_script(&[stone])?;
    let count = wallet
        .list_unspent()
        .filter(|u| excluded.contains(&u.outpoint))
        .count() as u64;
    let mut builder = wallet.build_tx();
    builder.ordering(TxOrdering::Untouched);
    builder.add_recipient(signer.script_pubkey(), Amount::from_sat(amount_sat));
    builder.add_recipient(
        recipient.script_pubkey(),
        Amount::from_sat(DEFAULT_POSTAGE_SAT),
    );
    builder.add_recipient(script, Amount::ZERO);
    builder.fee_rate(fee_rate);
    builder.unspendable(excluded.to_vec());
    let psbt = builder
        .finish()
        .map_err(|e| BuildError::CreateTx(e.to_string()))?;
    let fee = psbt
        .fee()
        .map_err(|e| BuildError::Fee(e.to_string()))?
        .to_sat();
    Ok(PreparedSpend::new(
        network.into(),
        signer.to_string(),
        amount_sat,
        fee,
        now,
        count,
        psbt,
    ))
}

pub fn check(
    tx: &Transaction,
    input_values: &[u64],
    signer: &Address,
    amount_sat: u64,
    wallet_owned: impl Fn(u32) -> bool,
) -> Result<u64, String> {
    let (vout, stones) = transaction_protostones(tx).map_err(|e| e.to_string())?;
    let mut message = call(WRAP_OPCODE).encode_cellpack();
    message.resize(message.len().div_ceil(MESSAGE_CHUNK) * MESSAGE_CHUNK, 0);
    let expected = Protostone {
        protocol_tag: ALKANES_PROTOCOL_TAG,
        message,
        pointer: Some(1),
        refund_pointer: Some(1),
        edicts: vec![],
    };
    if vout != 2
        || stones != [expected]
        || tx.output.len() < 3
        || tx.output[2].value != Amount::ZERO
    {
        return Err("wrap transaction has unexpected protocol data".into());
    }
    if tx.output[0].script_pubkey != signer.script_pubkey()
        || tx.output[0].value.to_sat() != amount_sat
        || wallet_owned(0)
    {
        return Err("wrap payment does not match the pinned signer and amount".into());
    }
    if tx.output[1].value.to_sat() != DEFAULT_POSTAGE_SAT
        || !wallet_owned(1)
        || (3..tx.output.len()).any(|i| !wallet_owned(i as u32))
    {
        return Err("wrap recipient or change does not belong to the wallet".into());
    }
    if tx.input.is_empty() || input_values.len() != tx.input.len() {
        return Err("missing wrap input values".into());
    }
    let unique: std::collections::HashSet<_> =
        tx.input.iter().map(|input| input.previous_output).collect();
    if unique.len() != tx.input.len() {
        return Err("duplicate wrap input".into());
    }
    let inputs = input_values
        .iter()
        .try_fold(0u64, |sum, v| sum.checked_add(*v))
        .ok_or("wrap input values overflow")?;
    let outputs = tx
        .output
        .iter()
        .try_fold(0u64, |sum, o| sum.checked_add(o.value.to_sat()))
        .ok_or("wrap output values overflow")?;
    inputs
        .checked_sub(outputs)
        .ok_or_else(|| "wrap outputs exceed inputs".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGNER: &str =
        "0a241a22512030685674457c3eaa6d2904a1340ab80296625e8c7313f54c6ae7c51318f5860710e1fc02";
    const MINT: &str = "0a5e0a0d0a060a0208201200120308864e122b0a262f7365656e2fffdbcfe2faaab58dcfe6363d4cad67bf68ccae88d3e55e0a685a440e35a85f7412010112200a0c2f746f74616c737570706c7912107acbb22f0100000000000000000000001093880f";
    const ZERO: &str = "0a0c0a0a0a060a02082012001200";

    #[test]
    fn signer_context_matches_the_reference_and_live_script_is_not_retweaked() {
        assert_eq!(
            hex::encode(context_request(&call(SIGNER_OPCODE), None, 880000, 0, 0)),
            "2080db352a03200067"
        );
        let bytes = hex::decode(SIGNER).unwrap();
        let address = signer_address(&bytes, Network::Testnet4).unwrap();
        assert_eq!(
            hex::encode(address.script_pubkey().as_bytes()),
            "512030685674457c3eaa6d2904a1340ab80296625e8c7313f54c6ae7c51318f58607"
        );
        assert!(signer_address(&bytes, Network::Bitcoin).is_err());
        assert!(signer_address(&[], Network::Testnet4).is_err());
        assert!(signer_address(&[0x0a, 0], Network::Testnet4).is_err());
        let mut bad = bytes.clone();
        bad.extend([0x1a, 1, b'x']);
        assert!(signer_address(&bad, Network::Testnet4).is_err());
        let mut duplicate = bytes.clone();
        duplicate.extend(bytes);
        assert!(signer_address(&duplicate, Network::Testnet4).is_err());
    }

    #[test]
    fn simulation_requires_positive_bounded_frbtc_and_no_error() {
        let bytes = hex::decode(MINT).unwrap();
        assert_eq!(predicted_frbtc(&bytes, 10000).unwrap(), 9990);
        assert!(predicted_frbtc(&bytes, 9989).is_err());
        assert!(predicted_frbtc(&hex::decode(ZERO).unwrap(), 10000).is_err());
        assert!(predicted_frbtc(&[], 10000).is_err());
        let mut error = bytes.clone();
        error.extend([0x1a, 1, b'x']);
        assert!(predicted_frbtc(&error, 10000).is_err());
        let mut wrong = bytes.clone();
        wrong[10] = 31; // target's block field
        assert!(predicted_frbtc(&wrong, 10000).is_err());
        let mut duplicate = bytes.clone();
        duplicate.extend(bytes);
        assert!(predicted_frbtc(&duplicate, 10000).is_err());
    }

    fn tx() -> (Transaction, Address) {
        let signer = signer_address(&hex::decode(SIGNER).unwrap(), Network::Testnet4).unwrap();
        let stone = Protostone {
            protocol_tag: 1,
            message: call(77).encode_cellpack(),
            pointer: Some(1),
            refund_pointer: Some(1),
            edicts: vec![],
        };
        let tx = Transaction {
            version: sats_core::bitcoin::transaction::Version::TWO,
            lock_time: sats_core::bitcoin::absolute::LockTime::ZERO,
            input: vec![sats_core::bitcoin::TxIn::default()],
            output: vec![
                sats_core::bitcoin::TxOut {
                    value: Amount::from_sat(10000),
                    script_pubkey: signer.script_pubkey(),
                },
                sats_core::bitcoin::TxOut {
                    value: Amount::from_sat(546),
                    script_pubkey: ScriptBuf::new(),
                },
                sats_core::bitcoin::TxOut {
                    value: Amount::ZERO,
                    script_pubkey: runestone_script(&[stone]).unwrap(),
                },
                sats_core::bitcoin::TxOut {
                    value: Amount::from_sat(9000),
                    script_pubkey: ScriptBuf::new(),
                },
            ],
        };
        (tx, signer)
    }

    #[test]
    fn checker_rejects_changed_payments_pointers_outputs_and_fees() {
        let (tx, signer) = tx();
        assert_eq!(
            check(&tx, &[20000], &signer, 10000, |v| v != 0).unwrap(),
            454
        );
        assert!(check(&tx, &[], &signer, 10000, |v| v != 0).is_err());
        assert!(check(&tx, &[1], &signer, 10000, |v| v != 0).is_err());
        assert!(check(&tx, &[20000], &signer, 10000, |v| v == 1).is_err());
        assert!(check(&tx, &[20000], &signer, 10000, |_| true).is_err());
        for change in 0..6 {
            let mut bad = tx.clone();
            match change {
                0 => bad.output[0].value = Amount::from_sat(9999),
                1 => bad.output[0].script_pubkey = ScriptBuf::new(),
                2 => bad.output[1].value = Amount::from_sat(547),
                3 => bad.output[2].value = Amount::from_sat(1),
                4 => bad.output.swap(0, 2),
                _ => {
                    let stone = Protostone {
                        protocol_tag: 1,
                        message: call(77).encode_cellpack(),
                        pointer: Some(0),
                        refund_pointer: Some(1),
                        edicts: vec![],
                    };
                    bad.output[2].script_pubkey = runestone_script(&[stone]).unwrap();
                }
            }
            assert!(
                check(&bad, &[20000], &signer, 10000, |v| v != 0).is_err(),
                "case {change}"
            );
        }
    }
}
