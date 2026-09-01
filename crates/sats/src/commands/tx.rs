//! Broadcast an explicit target: a raw transaction hex file, or a saved
//! transaction by txid or unique prefix. There is no hidden "newest
//! pending" default — `sats status` lists what can be broadcast.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use sats_core::bitcoin::{Network, Transaction, consensus};
use sats_core::plan::TransactionStatus;

use crate::provider::Services;
use crate::store::Store;
use crate::{ui, walletd};

pub fn broadcast(
    store: &Store,
    network: Network,
    services: &Services,
    target: &str,
    json: bool,
) -> Result<()> {
    let mut ctx = walletd::open(store, network)?;

    let path = Path::new(target);
    if path.exists() {
        let text =
            fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        let bytes = hex::decode(text.trim()).map_err(|e| anyhow!("not valid tx hex: {e}"))?;
        let tx: Transaction = consensus::encode::deserialize(&bytes)
            .map_err(|e| anyhow!("not a valid transaction: {e}"))?;
        let txid = services.broadcast(&mut ctx, &tx)?;
        report(json, &txid.to_string());
        return Ok(());
    }

    let mut record = store.load_transaction(ctx.net_name, target)?;

    match record.status {
        TransactionStatus::Pending => {}
        TransactionStatus::Broadcast => {
            bail!("transaction {} was already broadcast", record.txid)
        }
    }

    let txid = crate::spend::broadcast_record(store, &mut ctx, services, &mut record)?;
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
