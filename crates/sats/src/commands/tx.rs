//! Broadcast an explicit target: a raw transaction hex file, or a saved
//! transaction by txid or unique prefix. There is no hidden "newest
//! pending" default — `sats status` lists what can be broadcast.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use sats_core::bitcoin::{Network, Transaction, consensus};
use sats_core::plan::TransactionStatus;

use crate::provider::Services;
use crate::store::Store;
use crate::{ui, walletd};

pub fn broadcast(
    store: &Store,
    network: Network,
    resolve_services: impl FnOnce() -> Result<Services>,
    target: &str,
    json: bool,
) -> Result<()> {
    let net_name = crate::config::network_name(network);
    let path = Path::new(target);
    if path.exists() {
        let text =
            fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        let bytes = hex::decode(text.trim()).map_err(|e| anyhow!("not valid tx hex: {e}"))?;
        let tx: Transaction = consensus::encode::deserialize(&bytes)
            .map_err(|e| anyhow!("not a valid transaction: {e}"))?;
        let mut ctx = walletd::open(store, network)?;
        let txid = resolve_services()?.broadcast(&mut ctx, &tx)?;
        report(json, &txid.to_string());
        return Ok(());
    }

    let mut record = store.load_transaction(net_name, target)?;

    match record.status {
        TransactionStatus::Pending => {}
        TransactionStatus::Broadcast => {
            // Retry the local receipt write even if a previous invocation
            // broadcast successfully. No provider, replan, or signer is needed.
            crate::request::settle_broadcast(store, network, &record.txid)?;
            report(json, &record.txid);
            return Ok(());
        }
    }

    let mut ctx = walletd::open(store, network)?;
    let services = resolve_services()?;
    let txid = crate::spend::broadcast_record(store, &mut ctx, &services, &mut record)?;
    // An agent request signed earlier but never broadcast settles now.
    if let Err(err) = crate::request::settle_broadcast(store, network, &txid.to_string()) {
        eprintln!(
            "⚠ broadcast succeeded but the request record was not updated: {err:#}; retry: sats tx broadcast {txid}"
        );
    }
    report(json, &txid.to_string());
    Ok(())
}

fn report(json: bool, txid: &str) {
    if json {
        println!("{}", serde_json::json!({ "txid": txid }));
    } else {
        ui::ok(&format!("broadcast  {txid}"));
    }
}
