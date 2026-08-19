use anyhow::{Result, bail};
use sats_core::bitcoin::Network;
use sats_core::plan::PlanStatus;
use sats_core::signer::{LocalSigner, Signer};

use crate::commands::plan;
use crate::config::Config;
use crate::store::Store;
use crate::{keys, ui, walletd};

#[allow(clippy::too_many_arguments)]
pub fn run(
    store: &Store,
    config: &Config,
    network: Network,
    address: &str,
    amount: u64,
    fee_rate: Option<u64>,
    yes: bool,
    json: bool,
) -> Result<()> {
    let mut ctx = walletd::open(store, config, network)?;
    if let Err(err) = ctx.sync() {
        eprintln!("✗ sync failed — planning on cached state ({err:#})");
    }

    let mut plan = plan::build(&mut ctx, address, amount, fee_rate)?;
    ctx.persist()?;

    if !json {
        plan::print_block(&plan);
    }
    if !yes && !ui::confirm("Sign?", true)? {
        ui::dim("aborted");
        return Ok(());
    }

    let mut psbt = plan.psbt()?;
    let mut signer = LocalSigner::new(keys::unlock(store)?, network);
    if !signer.sign(&mut psbt)? {
        bail!("signer produced an unfinalized transaction");
    }
    plan.set_psbt(&psbt);
    plan.status = PlanStatus::Signed;
    if !json {
        ui::ok("signed");
    }

    let tx = plan.tx()?;
    match ctx.broadcast(&tx) {
        Ok(txid) => {
            plan.status = PlanStatus::Broadcast;
            store.save_plan(ctx.net_name, &plan)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "txid": txid.to_string(),
                        "amount_sat": plan.amount_sat,
                        "fee_sat": plan.fee_sat,
                        "total_sat": plan.total_sat(),
                    })
                );
            } else {
                ui::ok(&format!("broadcast  {txid}"));
            }
            Ok(())
        }
        Err(err) => {
            store.save_plan(ctx.net_name, &plan)?;
            bail!(
                "{err:#} — plan {} saved as signed, retry: sats broadcast",
                plan.id
            );
        }
    }
}
