//! Rebroadcast a transaction sats signed and saved, by txid or unique
//! prefix. There is no hidden "newest pending" default — `sats status`
//! lists what can be broadcast — and no way to broadcast a transaction
//! sats did not sign.

use anyhow::Result;
use sats_core::bitcoin::Network;
use sats_wallet::provider::Services;
use sats_wallet::store::Store;

use crate::ui;

pub fn broadcast(
    store: &Store,
    network: Network,
    resolve_services: impl FnOnce() -> Result<Services>,
    target: &str,
    json: bool,
) -> Result<()> {
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
