//! Broadcast an explicit target: a raw transaction hex file, or a saved
//! transaction by txid or unique prefix. There is no hidden "newest
//! pending" default — `sats status` lists what can be broadcast.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use sats_core::bitcoin::{Network, Transaction, consensus};
use sats_wallet::provider::Services;
use sats_wallet::store::Store;
use sats_wallet::walletd;

use crate::ui;

pub fn broadcast(
    store: &Store,
    network: Network,
    resolve_services: impl FnOnce() -> Result<Services>,
    target: &str,
    json: bool,
) -> Result<()> {
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

    let done = sats_wallet::spend::rebroadcast(store, network, resolve_services, target)?;
    if let Some(err) = &done.receipt_error {
        eprintln!(
            "⚠ broadcast succeeded but the request record was not updated: {err:#}; retry: sats tx broadcast {}",
            done.txid
        );
    }
    report(json, &done.txid);
    Ok(())
}

fn report(json: bool, txid: &str) {
    if json {
        println!("{}", serde_json::json!({ "txid": txid }));
    } else {
        ui::ok(&format!("broadcast  {txid}"));
    }
}
