use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use sats_core::bitcoin::{Network, Transaction, consensus};
use sats_core::plan::{LegacyPlan, LegacyPlanStatus, TransactionRecord, TransactionStatus};

use crate::provider::Services;
use crate::store::Store;
use crate::{ui, walletd};

pub fn run(
    store: &Store,
    network: Network,
    services: &Services,
    transaction_id: Option<String>,
    legacy_plan_id: Option<String>,
    tx_file: Option<&Path>,
    json: bool,
) -> Result<()> {
    let mut ctx = walletd::open(store, network)?;

    if let Some(file) = tx_file {
        let text =
            fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
        let bytes = hex::decode(text.trim()).map_err(|e| anyhow!("not valid tx hex: {e}"))?;
        let tx: Transaction = consensus::encode::deserialize(&bytes)
            .map_err(|e| anyhow!("not a valid transaction: {e}"))?;
        let txid = services.broadcast(&mut ctx, &tx)?;
        report(json, &txid.to_string());
        return Ok(());
    }

    let mut record = if let Some(id) = transaction_id {
        store.load_transaction(ctx.net_name, &id)?
    } else if let Some(id) = legacy_plan_id {
        match store.load_transaction(ctx.net_name, &id) {
            Ok(record) => record,
            Err(err) => {
                let legacy_path = store
                    .legacy_plans_dir(ctx.net_name)
                    .join(format!("{id}.json"));
                if !legacy_path.exists() {
                    return Err(err);
                }
                migrate_legacy_plan(
                    store,
                    ctx.net_name,
                    store.load_legacy_plan(ctx.net_name, &id)?,
                )?
            }
        }
    } else if let Some(record) =
        store.latest_transaction(ctx.net_name, TransactionStatus::Pending)?
    {
        record
    } else if let Some(plan) = store.latest_legacy_plan(ctx.net_name, LegacyPlanStatus::Signed)? {
        migrate_legacy_plan(store, ctx.net_name, plan)?
    } else {
        bail!("no pending transactions — run: sats send, or sats plan then sats sign");
    };

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

fn migrate_legacy_plan(
    store: &Store,
    network: &str,
    plan: LegacyPlan,
) -> Result<TransactionRecord> {
    match plan.status {
        LegacyPlanStatus::Unsigned => {
            bail!(
                "plan {} is not signed — run: sats sign --plan {}",
                plan.id,
                plan.id
            )
        }
        LegacyPlanStatus::Signed => {}
        LegacyPlanStatus::Broadcast => bail!("plan {} was already broadcast", plan.id),
    }
    let source_id = plan.id.clone();
    let record = plan.into_transaction()?;
    store.save_transaction(network, &record)?;
    store.delete_legacy_plan(network, &source_id)?;
    Ok(record)
}

fn report(json: bool, txid: &str) {
    if json {
        println!("{}", serde_json::json!({ "txid": txid }));
    } else {
        ui::ok(&format!("broadcast  {txid}"));
    }
}
