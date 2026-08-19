use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use sats_core::bitcoin::{Network, Transaction, consensus};
use sats_core::plan::PlanStatus;

use crate::config::Config;
use crate::store::Store;
use crate::{ui, walletd};

pub fn run(
    store: &Store,
    config: &Config,
    network: Network,
    plan_id: Option<String>,
    tx_file: Option<&Path>,
    json: bool,
) -> Result<()> {
    let mut ctx = walletd::open(store, config, network)?;

    if let Some(file) = tx_file {
        let text =
            fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
        let bytes = hex::decode(text.trim()).map_err(|e| anyhow!("not valid tx hex: {e}"))?;
        let tx: Transaction = consensus::encode::deserialize(&bytes)
            .map_err(|e| anyhow!("not a valid transaction: {e}"))?;
        let txid = ctx.broadcast(&tx)?;
        report(json, &txid.to_string());
        return Ok(());
    }

    let mut plan = match plan_id {
        Some(id) => store.load_plan(ctx.net_name, &id)?,
        None => store
            .latest_plan(ctx.net_name, PlanStatus::Signed)?
            .context("no signed plans — run: sats sign")?,
    };
    match plan.status {
        PlanStatus::Signed => {}
        PlanStatus::Unsigned => bail!("plan {} is not signed — run: sats sign", plan.id),
        PlanStatus::Broadcast => bail!("plan {} was already broadcast", plan.id),
    }

    let tx = plan.tx()?;
    let txid = ctx.broadcast(&tx)?;
    plan.status = PlanStatus::Broadcast;
    store.save_plan(ctx.net_name, &plan)?;
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
