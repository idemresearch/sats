//! Asset transfers: send an exact amount of one alkane to a recipient,
//! with everything else returning to the wallet.
//!
//! The transaction has a fixed shape:
//!
//! | vout | output |
//! |---|---|
//! | 0 | the runestone: one alkanes protostone, no message, one edict |
//! | 1 | the recipient, carrying postage; the edict's target |
//! | 2 | a wallet address, carrying postage; the pointer and refund |
//! | 3 | BTC change, when there is any |
//!
//! [`summarize`] does not trust that shape. It decodes the protostone
//! from the transaction and replays the indexer's transfer rules
//! (`index_protostones` and `process_edict` in
//! `crates/protorune/src/lib.rs` @ 62511e9): every alkane on the inputs
//! enters the first alkanes protostone; each edict moves
//! `min(amount, available)`; whatever is left goes to the pointer. A
//! transaction is accepted only if the recipient ends up with exactly the
//! intended amount of the intended asset, nothing else, and every other
//! alkane lands on a wallet output.
//!
//! What this cannot see: the input balances come from the indexer, so a
//! lying indexer can hide an asset. Ordinals runes are not alkanes and
//! are not modeled; a runestone with no rune fields sends any runes on
//! the inputs to the first non-OP_RETURN output, the recipient.

use std::collections::{BTreeMap, HashSet};

use sats_core::bdk_wallet::{TxOrdering, Wallet};
use sats_core::bitcoin::{Address, Amount, FeeRate, OutPoint, Script, Transaction};
use sats_core::plan::PreparedSpend;

use crate::build::BuildError;
use crate::decode::{DecodeError, transaction_protostones};
use crate::id::AlkaneId;
use crate::protostone::{ALKANES_PROTOCOL_TAG, Edict, Protostone, runestone_script};

pub const RECIPIENT_VOUT: u32 = 1;
pub const WALLET_VOUT: u32 = 2;

/// Asset amounts by id.
pub type Sheet = BTreeMap<AlkaneId, u128>;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TransferError {
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error("refusing this transfer: {0}")]
    Refused(String),
}

fn refused(message: impl Into<String>) -> TransferError {
    TransferError::Refused(message.into())
}

/// The protostone for a transfer: the edict sends `amount` of `asset` to
/// the recipient; pointer and refund return the rest to the wallet.
pub fn transfer_stone(asset: AlkaneId, amount: u128) -> Protostone {
    Protostone {
        protocol_tag: ALKANES_PROTOCOL_TAG,
        message: Vec::new(),
        pointer: Some(WALLET_VOUT),
        refund_pointer: Some(WALLET_VOUT),
        edicts: vec![Edict {
            id: asset,
            amount,
            output: RECIPIENT_VOUT,
        }],
    }
}

/// Build the unsigned transfer. `asset_inputs` are spent whatever their
/// value (they carry the asset); BDK adds BTC inputs for the postage and
/// fee, never one in `unspendable`.
#[allow(clippy::too_many_arguments)]
pub fn build_transfer_plan(
    wallet: &mut Wallet,
    asset: AlkaneId,
    amount: u128,
    recipient: &Address,
    wallet_address: &Address,
    asset_inputs: &[OutPoint],
    postage_sat: u64,
    fee_rate: FeeRate,
    unspendable: &[OutPoint],
    network: &str,
    now_unix: u64,
) -> Result<PreparedSpend, BuildError> {
    if amount == 0 {
        // An edict amount of 0 means "everything": never write one.
        return Err(BuildError::CreateTx(
            "transfer amount must be positive".into(),
        ));
    }
    if asset_inputs.is_empty() {
        return Err(BuildError::CreateTx("no outputs carry this asset".into()));
    }
    let runestone = runestone_script(&[transfer_stone(asset, amount)])?;
    let required: HashSet<OutPoint> = asset_inputs.iter().copied().collect();
    let unspendable: Vec<OutPoint> = unspendable
        .iter()
        .copied()
        .filter(|op| !required.contains(op))
        .collect();
    let excluded_utxos = {
        let excluded: HashSet<OutPoint> = unspendable.iter().copied().collect();
        wallet
            .list_unspent()
            .filter(|u| excluded.contains(&u.outpoint))
            .count() as u64
    };

    let mut builder = wallet.build_tx();
    builder.ordering(TxOrdering::Untouched);
    builder.add_recipient(runestone, Amount::ZERO);
    builder.add_recipient(recipient.script_pubkey(), Amount::from_sat(postage_sat));
    builder.add_recipient(
        wallet_address.script_pubkey(),
        Amount::from_sat(postage_sat),
    );
    builder
        .add_utxos(asset_inputs)
        .map_err(|e| BuildError::CreateTx(e.to_string()))?;
    builder.unspendable(unspendable);
    builder.fee_rate(fee_rate);
    let psbt = builder
        .finish()
        .map_err(|e| BuildError::CreateTx(e.to_string()))?;
    let fee_sat = psbt
        .fee()
        .map_err(|e| BuildError::Fee(e.to_string()))?
        .to_sat();
    Ok(PreparedSpend::new(
        network.to_string(),
        recipient.to_string(),
        postage_sat,
        fee_sat,
        now_unix,
        excluded_utxos,
        psbt,
    ))
}

/// Where every alkane on the inputs ends up, by output index, under the
/// indexer's rules — for the narrow shape a transfer may take: one
/// alkanes protostone, no message, an explicit pointer, edicts to real
/// outputs. Anything else is refused rather than modeled.
pub fn transfer_effects(
    tx: &Transaction,
    inputs: &[Sheet],
) -> Result<BTreeMap<u32, Sheet>, TransferError> {
    if inputs.len() != tx.input.len() {
        return Err(refused("input balances do not match the inputs"));
    }
    let (_, stones) = transaction_protostones(tx)?;
    let [stone] = stones.as_slice() else {
        return Err(refused(format!(
            "expected one protostone, found {}",
            stones.len()
        )));
    };
    if stone.protocol_tag != ALKANES_PROTOCOL_TAG {
        return Err(refused(format!(
            "protostone for protocol {}, not alkanes",
            stone.protocol_tag
        )));
    }
    if !stone.message.is_empty() {
        return Err(refused("the protostone calls a contract"));
    }
    let real_output = |vout: u32| {
        tx.output
            .get(vout as usize)
            .is_some_and(|out| !out.script_pubkey.is_op_return())
    };
    let pointer = stone
        .pointer
        .ok_or_else(|| refused("the protostone has no pointer"))?;
    if !real_output(pointer) {
        return Err(refused(format!(
            "pointer {pointer} is not a spendable output"
        )));
    }

    let mut pool = Sheet::new();
    for sheet in inputs {
        for (id, amount) in sheet {
            let slot = pool.entry(*id).or_default();
            *slot = slot
                .checked_add(*amount)
                .ok_or_else(|| refused("input balances overflow"))?;
        }
    }
    let mut outputs: BTreeMap<u32, Sheet> = BTreeMap::new();
    fn credit(outputs: &mut BTreeMap<u32, Sheet>, vout: u32, id: AlkaneId, amount: u128) {
        if amount > 0 {
            *outputs.entry(vout).or_default().entry(id).or_default() += amount;
        }
    }
    for edict in &stone.edicts {
        if edict.id.block == 0 && edict.id.tx != 0 {
            return Err(refused("invalid edict id"));
        }
        if edict.amount == 0 {
            return Err(refused("an edict moves the whole balance"));
        }
        if !real_output(edict.output) {
            return Err(refused(format!(
                "edict targets output {}, which is not a spendable output",
                edict.output
            )));
        }
        let available = pool.entry(edict.id).or_default();
        let moved = edict.amount.min(*available);
        *available -= moved;
        credit(&mut outputs, edict.output, edict.id, moved);
    }
    for (id, amount) in pool {
        credit(&mut outputs, pointer, id, amount);
    }
    Ok(outputs)
}

/// A transfer's verified effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferSummary {
    pub recipient_vout: u32,
    /// Sats paid to the recipient's output.
    pub recipient_sat: u64,
    /// Sats leaving the wallet other than the fee (the recipient's postage).
    pub external_sat: u64,
    pub fee_sat: u64,
    /// Every alkane that stays in the wallet, the asset's remainder included.
    pub returned: Sheet,
}

/// Check that `tx` sends exactly `amount` of `asset` to the single output
/// paying `recipient`, nothing else to it, and every other alkane to an
/// output `wallet_owned` accepts. `input_sat` and `inputs` describe the
/// spent outputs, in input order.
pub fn summarize(
    tx: &Transaction,
    input_sat: &[u64],
    inputs: &[Sheet],
    asset: AlkaneId,
    amount: u128,
    recipient: &Script,
    wallet_owned: impl Fn(u32) -> bool,
) -> Result<TransferSummary, TransferError> {
    let effects = transfer_effects(tx, inputs)?;

    let paying: Vec<u32> = (0..tx.output.len() as u32)
        .filter(|vout| tx.output[*vout as usize].script_pubkey.as_script() == recipient)
        .collect();
    let [recipient_vout] = paying.as_slice() else {
        return Err(refused(format!(
            "{} outputs pay the recipient (expected one)",
            paying.len()
        )));
    };
    let received = effects.get(recipient_vout).cloned().unwrap_or_default();
    let expected = Sheet::from([(asset, amount)]);
    if received != expected {
        let got = received.get(&asset).copied().unwrap_or(0);
        return Err(refused(if got < amount && received.len() <= 1 {
            format!(
                "the recipient would receive {got} of {asset}, not {amount}: the inputs \
                 don't hold enough"
            )
        } else {
            format!("the recipient would receive {received:?}, not exactly {amount} of {asset}")
        }));
    }
    let mut returned = Sheet::new();
    for (vout, sheet) in &effects {
        if vout == recipient_vout {
            continue;
        }
        if !wallet_owned(*vout) {
            return Err(refused(format!(
                "output {vout} would receive alkanes but is not this wallet's"
            )));
        }
        for (id, amount) in sheet {
            *returned.entry(*id).or_default() += amount;
        }
    }

    if input_sat.len() != tx.input.len() {
        return Err(refused("input values do not match the inputs"));
    }
    let total_in = input_sat
        .iter()
        .try_fold(0u64, |acc, v| acc.checked_add(*v))
        .ok_or_else(|| refused("input values overflow"))?;
    let mut total_out = 0u64;
    let mut external_sat = 0u64;
    for (vout, out) in tx.output.iter().enumerate() {
        let value = out.value.to_sat();
        if out.script_pubkey.is_op_return() && value != 0 {
            return Err(refused("the OP_RETURN output carries sats"));
        }
        total_out = total_out
            .checked_add(value)
            .ok_or_else(|| refused("output values overflow"))?;
        if !out.script_pubkey.is_op_return() && !wallet_owned(vout as u32) {
            external_sat += value;
        }
    }
    let fee_sat = total_in
        .checked_sub(total_out)
        .ok_or_else(|| refused("outputs exceed inputs"))?;
    Ok(TransferSummary {
        recipient_vout: *recipient_vout,
        recipient_sat: tx.output[*recipient_vout as usize].value.to_sat(),
        external_sat,
        fee_sat,
        returned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sats_core::bitcoin::hashes::Hash;
    use sats_core::bitcoin::{ScriptBuf, TxIn, TxOut, Txid, absolute, transaction};

    const FRBTC: AlkaneId = AlkaneId { block: 32, tx: 0 };
    const OTHER: AlkaneId = AlkaneId { block: 2, tx: 9 };

    fn script(byte: u8) -> ScriptBuf {
        // A stand-in P2TR scriptPubKey per byte.
        let mut bytes = vec![0x51, 0x20];
        bytes.extend([byte; 32]);
        ScriptBuf::from_bytes(bytes)
    }

    fn recipient() -> ScriptBuf {
        script(0xee)
    }

    fn input(n: u8) -> TxIn {
        TxIn {
            previous_output: OutPoint {
                txid: Txid::from_byte_array([n; 32]),
                vout: 0,
            },
            ..Default::default()
        }
    }

    /// A transfer as `build_transfer_plan` lays it out, with `stones` in
    /// the runestone.
    fn tx_with(stones: &[Protostone], inputs: usize) -> Transaction {
        let out = |value: u64, script_pubkey: ScriptBuf| TxOut {
            value: Amount::from_sat(value),
            script_pubkey,
        };
        Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: (0..inputs as u8).map(input).collect(),
            output: vec![
                out(0, runestone_script(stones).unwrap()),
                out(546, recipient()),
                out(546, script(1)),
                out(10_000, script(2)),
            ],
        }
    }

    fn mine(vout: u32) -> bool {
        vout == 2 || vout == 3
    }

    fn sheet(entries: &[(AlkaneId, u128)]) -> Sheet {
        entries.iter().copied().collect()
    }

    #[test]
    fn exact_amount_reaches_the_recipient_and_the_rest_returns() {
        let tx = tx_with(&[transfer_stone(FRBTC, 40_000)], 2);
        let inputs = [
            sheet(&[(FRBTC, 100_000), (OTHER, 7)]),
            Sheet::new(), // a plain BTC input
        ];
        let summary = summarize(
            &tx,
            &[546, 20_000],
            &inputs,
            FRBTC,
            40_000,
            &recipient(),
            mine,
        )
        .unwrap();
        assert_eq!(summary.recipient_vout, 1);
        assert_eq!(summary.recipient_sat, 546);
        assert_eq!(summary.external_sat, 546);
        assert_eq!(summary.fee_sat, 20_546 - 11_092);
        assert_eq!(summary.returned, sheet(&[(FRBTC, 60_000), (OTHER, 7)]));
    }

    #[test]
    fn an_underfunded_transfer_is_refused_not_shortened() {
        // The indexer would quietly move only what exists.
        let tx = tx_with(&[transfer_stone(FRBTC, 40_000)], 1);
        let err = summarize(
            &tx,
            &[546],
            &[sheet(&[(FRBTC, 39_999)])],
            FRBTC,
            40_000,
            &recipient(),
            mine,
        )
        .unwrap_err();
        assert!(err.to_string().contains("receive 39999"), "{err}");
    }

    #[test]
    fn a_transaction_built_for_another_asset_or_amount_is_refused() {
        let tx = tx_with(&[transfer_stone(OTHER, 5)], 1);
        let inputs = [sheet(&[(FRBTC, 10), (OTHER, 10)])];
        assert!(summarize(&tx, &[546], &inputs, FRBTC, 5, &recipient(), mine).is_err());
        let tx = tx_with(&[transfer_stone(FRBTC, 6)], 1);
        assert!(summarize(&tx, &[546], &inputs, FRBTC, 5, &recipient(), mine).is_err());
    }

    #[test]
    fn leftovers_pointed_outside_the_wallet_are_refused() {
        let mut stone = transfer_stone(FRBTC, 5);
        stone.pointer = Some(1); // the recipient gets the remainder too
        let tx = tx_with(&[stone], 1);
        let inputs = [sheet(&[(FRBTC, 10)])];
        assert!(summarize(&tx, &[546], &inputs, FRBTC, 5, &recipient(), mine).is_err());

        // A pointer at an output the wallet doesn't own.
        let not_mine = |vout: u32| vout == 3;
        let tx = tx_with(&[transfer_stone(FRBTC, 5)], 1);
        let err = summarize(&tx, &[546], &inputs, FRBTC, 5, &recipient(), not_mine).unwrap_err();
        assert!(err.to_string().contains("not this wallet's"), "{err}");
    }

    #[test]
    fn shapes_sats_never_builds_are_refused() {
        let inputs = [sheet(&[(FRBTC, 10)])];
        let check = |stones: &[Protostone]| {
            summarize(
                &tx_with(stones, 1),
                &[546],
                &inputs,
                FRBTC,
                5,
                &recipient(),
                mine,
            )
        };
        // A contract call alongside the transfer.
        let mut call = transfer_stone(FRBTC, 5);
        call.message = vec![2, 1, 77];
        assert!(check(&[call]).is_err());
        // Two protostones.
        assert!(check(&[transfer_stone(FRBTC, 5), transfer_stone(FRBTC, 5)]).is_err());
        // No pointer: the indexer would fall back to the first output.
        let mut no_pointer = transfer_stone(FRBTC, 5);
        no_pointer.pointer = None;
        assert!(check(&[no_pointer]).is_err());
        // An edict to the OP_RETURN (a burn) or past the outputs.
        for output in [0, 4, 99] {
            let mut stone = transfer_stone(FRBTC, 5);
            stone.edicts[0].output = output;
            assert!(check(&[stone]).is_err(), "output {output}");
        }
        // A whole-balance edict.
        let mut all = transfer_stone(FRBTC, 5);
        all.edicts[0].amount = 0;
        assert!(check(&[all]).is_err());
    }

    #[test]
    fn the_recipient_must_be_paid_by_exactly_one_output() {
        let mut tx = tx_with(&[transfer_stone(FRBTC, 5)], 1);
        tx.output[3].script_pubkey = recipient();
        let inputs = [sheet(&[(FRBTC, 10)])];
        assert!(summarize(&tx, &[20_000], &inputs, FRBTC, 5, &recipient(), mine).is_err());
    }

    #[test]
    fn an_op_return_carrying_sats_is_refused() {
        let mut tx = tx_with(&[transfer_stone(FRBTC, 5)], 1);
        tx.output[0].value = Amount::from_sat(1);
        let inputs = [sheet(&[(FRBTC, 10)])];
        assert!(summarize(&tx, &[20_000], &inputs, FRBTC, 5, &recipient(), mine).is_err());
    }
}
