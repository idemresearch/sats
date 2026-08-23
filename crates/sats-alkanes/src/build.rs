//! Compose the alkanes execute transaction on a caller-owned wallet.
//!
//! Mirrors `sats_core::engine::build_plan`'s contract: pure of IO, the
//! wallet's UTXO view is whatever the caller last synced, and protection
//! facts arrive as plain `unspendable` outpoints — a guarantee that holds
//! because nothing here calls `TxBuilder::add_utxo`.

use std::collections::HashSet;

use sats_core::bdk_wallet::{TxOrdering, Wallet};
use sats_core::bitcoin::{Address, Amount, FeeRate, OutPoint};
use sats_core::plan::PreparedSpend;

use crate::call::AlkaneCall;
use crate::protostone::{ALKANES_PROTOCOL_TAG, EncodeError, Protostone, runestone_script};

/// Default sats carried by the pointer output. 546 is deliberate: the
/// wallet's own dust heuristic then protects the resulting asset-bearing
/// UTXO from later coin selection.
pub const DEFAULT_POSTAGE_SAT: u64 = 546;

/// The pointer output's index: output 0 is the runestone, output 1 the
/// postage the protostone points at, change follows.
const POINTER_VOUT: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error(transparent)]
    Encode(#[from] EncodeError),
    #[error("cannot create transaction: {0}")]
    CreateTx(String),
    #[error("cannot compute fee: {0}")]
    Fee(String),
}

/// Build the execute transaction: output 0 carries the runestone
/// embedding one alkanes protostone (pointer and refund both aimed at
/// output 1), output 1 pays `postage_sat` back to the wallet's own
/// `pointer_address`, and change follows. `TxOrdering::Untouched` is
/// mandatory — protostone pointers are vout indices, so nothing may
/// shuffle the outputs. Reveals nothing itself, but coin selection uses
/// a change address: callers with persistence should persist afterwards.
#[allow(clippy::too_many_arguments)]
pub fn build_execute_plan(
    wallet: &mut Wallet,
    call: &AlkaneCall,
    pointer_address: &Address,
    postage_sat: u64,
    fee_rate: FeeRate,
    unspendable: &[OutPoint],
    network: &str,
    now_unix: u64,
) -> Result<PreparedSpend, BuildError> {
    let stone = Protostone {
        protocol_tag: ALKANES_PROTOCOL_TAG,
        message: call.encode_cellpack(),
        pointer: Some(POINTER_VOUT),
        refund_pointer: Some(POINTER_VOUT),
    };
    let runestone = runestone_script(&[stone])?;

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
    builder.add_recipient(
        pointer_address.script_pubkey(),
        Amount::from_sat(postage_sat),
    );
    builder.fee_rate(fee_rate);
    builder.unspendable(unspendable.to_vec());
    let psbt = builder
        .finish()
        .map_err(|e| BuildError::CreateTx(e.to_string()))?;
    let fee_sat = psbt
        .fee()
        .map_err(|e| BuildError::Fee(e.to_string()))?
        .to_sat();
    Ok(PreparedSpend::new(
        network.to_string(),
        format!("alkanes:{}", call.target),
        postage_sat,
        fee_sat,
        now_unix,
        excluded_utxos,
        psbt,
    ))
}
