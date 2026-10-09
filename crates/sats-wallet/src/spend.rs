//! Shared spend execution: the signing and broadcast tails used by every
//! surface (human send, PSBT workflows, MCP agent sends). Keeping these in
//! one place keeps the safety ordering — persist before broadcast, never
//! lose a signed transaction — identical everywhere.

use anyhow::{Result, anyhow};
use bdk_wallet::bip39::Mnemonic;
use sats_core::bitcoin::{Network, Psbt, Txid};
use sats_core::plan::{PreparedSpend, TransactionRecord, TransactionStatus};
use sats_core::signer::{LocalSigner, Signer};

use crate::config::network_name;
use crate::provider::Services;
use crate::store::Store;
use crate::walletd::{self, WalletCtx};

/// Sign a prepared spend's PSBT to finality. An error means this call
/// returned no finalized transaction, not that no signature exists: the
/// signer ran. Nothing that holds a budget reservation may refund on it;
/// agent requests sign only through `request::execute`, which treats any
/// failure from the signer on as unresolved.
fn sign_psbt(prepared: &PreparedSpend, mnemonic: Mnemonic, network: Network) -> Result<Psbt> {
    let mut psbt = prepared.psbt().clone();
    let mut signer = LocalSigner::new(mnemonic, network);
    if !signer.sign(&mut psbt)? {
        return Err(anyhow!("signer produced an unfinalized transaction"));
    }
    Ok(psbt)
}

/// Sign a prepared spend for a human send and produce the durable
/// transaction record. The caller must persist the record before any
/// broadcast attempt. Not for agent requests: see [`sign_psbt`].
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
        log::warn!("broadcast succeeded but the local record was not updated: {err:#}");
    }
    Ok(txid)
}

/// What [`rebroadcast`] did with a saved transaction.
#[derive(Debug)]
pub struct Rebroadcast {
    pub txid: String,
    /// The transaction is out, but the agent request it was signed for
    /// could not be updated. Rebroadcasting the same txid repairs it
    /// without resolving a provider.
    pub receipt_error: Option<anyhow::Error>,
}

/// Broadcast a saved transaction, by txid or unique prefix, and settle the
/// agent request it was signed for. A transaction already recorded as
/// broadcast only has that receipt repaired: no provider is resolved, and
/// nothing is replanned or signed.
pub fn rebroadcast(
    store: &Store,
    network: Network,
    resolve_services: impl FnOnce() -> Result<Services>,
    txid_or_prefix: &str,
) -> Result<Rebroadcast> {
    let net_name = network_name(network);
    let mut record = store.load_transaction(net_name, txid_or_prefix)?;
    match record.status {
        TransactionStatus::Pending => {}
        TransactionStatus::Broadcast => {
            crate::request::settle_broadcast(store, network, &record.txid)?;
            return Ok(Rebroadcast {
                txid: record.txid,
                receipt_error: None,
            });
        }
    }
    let mut ctx = walletd::open(store, network)?;
    let services = resolve_services()?;
    let txid = broadcast_record(store, &mut ctx, &services, &mut record)?.to_string();
    // An agent request signed earlier but never broadcast settles now.
    let receipt_error = crate::request::settle_broadcast(store, network, &txid).err();
    Ok(Rebroadcast {
        txid,
        receipt_error,
    })
}
