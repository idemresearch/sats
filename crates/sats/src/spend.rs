//! Shared spend execution: the signing and broadcast tails used by every
//! surface (human send, PSBT workflows, MCP agent sends). Keeping these in
//! one place keeps the safety ordering — persist before broadcast, never
//! lose a signed transaction — identical everywhere.

use anyhow::{Result, anyhow};
use bdk_wallet::bip39::Mnemonic;
use sats_core::bitcoin::{Network, Psbt, Txid};
use sats_core::plan::{PreparedSpend, TransactionRecord};
use sats_core::signer::{LocalSigner, Signer};

use crate::provider::Services;
use crate::store::Store;
use crate::walletd::WalletCtx;

/// Sign a prepared spend's PSBT to finality. An error does not prove that
/// no signature exists. Agent budget reservations are handled by the request
/// executor, which never refunds after invoking the signer.
pub fn sign_psbt(prepared: &PreparedSpend, mnemonic: Mnemonic, network: Network) -> Result<Psbt> {
    let mut psbt = prepared.psbt().clone();
    let mut signer = LocalSigner::new(mnemonic, network);
    if !signer.sign(&mut psbt)? {
        return Err(anyhow!("signer produced an unfinalized transaction"));
    }
    Ok(psbt)
}

/// Sign a prepared spend and produce the durable transaction record.
/// The caller must persist the record before any broadcast attempt.
pub fn sign_to_record(
    prepared: PreparedSpend,
    mnemonic: Mnemonic,
    network: Network,
) -> Result<TransactionRecord> {
    let psbt = sign_psbt(&prepared, mnemonic, network)?;
    Ok(prepared.into_transaction(psbt)?)
}

/// Broadcast an already-persisted pending record, then mark and re-save it.
/// A save failure after a successful broadcast is only a warning: the
/// irreversible act succeeded and the transaction is out of our hands.
pub fn broadcast_record(
    store: &Store,
    ctx: &mut WalletCtx,
    services: &Services,
    record: &mut TransactionRecord,
) -> Result<Txid> {
    let tx = record.tx()?;
    let txid = services.broadcast(ctx, &tx)?;
    record.mark_broadcast();
    if let Err(err) = store.save_transaction(ctx.net_name, record) {
        eprintln!("⚠ broadcast succeeded but the local record was not updated: {err:#}");
    }
    Ok(txid)
}
