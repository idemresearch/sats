//! Transaction planning on a caller-owned wallet. Pure of IO: the wallet's
//! UTXO view is whatever the caller last synced.

use bdk_wallet::Wallet;
use bdk_wallet::bitcoin::{Address, Amount, FeeRate};

use crate::error::PlanError;
use crate::plan::{Plan, PlanStatus};

/// Build an unsigned spend plan. Reveals a change address on the wallet, so
/// callers with persistence should persist afterwards.
pub fn build_plan(
    wallet: &mut Wallet,
    recipient: &Address,
    amount: Amount,
    fee_rate: FeeRate,
    network: &str,
    now_unix: u64,
) -> Result<Plan, PlanError> {
    let mut builder = wallet.build_tx();
    builder.add_recipient(recipient.script_pubkey(), amount);
    builder.fee_rate(fee_rate);
    let psbt = builder.finish().map_err(Box::new)?;

    let fee_sat = psbt.fee()?.to_sat();
    let id = psbt.unsigned_tx.compute_txid().to_string()[..8].to_string();
    Ok(Plan {
        id,
        network: network.to_string(),
        recipient: recipient.to_string(),
        amount_sat: amount.to_sat(),
        fee_sat,
        created_at: now_unix,
        status: PlanStatus::Unsigned,
        psbt: psbt.to_string(),
    })
}
