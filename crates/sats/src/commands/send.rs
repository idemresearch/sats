use anyhow::{Result, bail};
use sats_core::bitcoin::Network;

use crate::commands::plan;
use crate::provider::Services;
use crate::store::Store;
use crate::{keys, spend, ui, walletd};

pub fn run(
    store: &Store,
    network: Network,
    services: &Services,
    req: &plan::PlanRequest,
    yes: bool,
    json: bool,
) -> Result<()> {
    let mut ctx = walletd::open(store, network)?;
    let prepared = plan::build(&mut ctx, services, req)?;
    ctx.persist()?;

    if !json {
        plan::print_block(&prepared);
    }
    if !yes && !ui::confirm("Sign?", true)? {
        ui::dim("aborted");
        return Ok(());
    }

    let mut record = spend::sign_to_record(prepared, keys::unlock(store)?, network, None)?;
    // Persist before any network call. A crash or lost provider response can
    // never strand the only copy of a signed transaction.
    store.save_transaction(ctx.net_name, &record)?;
    if !json {
        ui::ok("signed");
    }

    match spend::broadcast_record(store, &mut ctx, services, &mut record) {
        Ok(txid) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "txid": txid.to_string(),
                        "amount_sat": record.amount_sat,
                        "fee_sat": record.fee_sat,
                        "total_sat": record.total_sat(),
                    })
                );
            } else {
                ui::ok(&format!("broadcast  {txid}"));
            }
            Ok(())
        }
        Err(err) => {
            bail!(
                "{err:#} — transaction {} saved, retry: sats broadcast --transaction {}",
                record.txid,
                record.txid
            );
        }
    }
}
