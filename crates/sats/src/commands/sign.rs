use std::fs;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use sats_core::bitcoin::{Network, Psbt};
use sats_core::plan::PlanStatus;
use sats_core::signer::{LocalSigner, Signer};

use crate::config::network_name;
use crate::store::Store;
use crate::{keys, ui};

pub fn run(
    store: &Store,
    network: Network,
    plan_id: Option<String>,
    psbt_file: Option<&Path>,
    json: bool,
) -> Result<()> {
    if let Some(file) = psbt_file {
        return sign_file(store, network, file);
    }

    let net_name = network_name(network);
    let mut plan = match plan_id {
        Some(id) => store.load_plan(net_name, &id)?,
        None => store
            .latest_plan(net_name, PlanStatus::Unsigned)?
            .context("no unsigned plans — run: sats plan")?,
    };
    match plan.status {
        PlanStatus::Unsigned => {}
        PlanStatus::Signed => bail!("plan {} is already signed — run: sats broadcast", plan.id),
        PlanStatus::Broadcast => bail!("plan {} was already broadcast", plan.id),
    }

    let mut psbt = plan.psbt()?;
    let mut signer = LocalSigner::new(keys::unlock(store)?, network);
    if !signer.sign(&mut psbt)? {
        bail!("signer produced an unfinalized transaction");
    }
    plan.set_psbt(&psbt);
    plan.status = PlanStatus::Signed;
    store.save_plan(net_name, &plan)?;

    if json {
        println!(
            "{}",
            serde_json::json!({ "id": plan.id, "status": "signed" })
        );
    } else {
        ui::ok(&format!("signed  {}", plan.id));
        ui::dim("next: sats broadcast");
    }
    Ok(())
}

/// Interop path: sign an external PSBT file (base64 text or binary) and
/// write `<name>.signed.psbt` next to it.
fn sign_file(store: &Store, network: Network, file: &Path) -> Result<()> {
    let bytes = fs::read(file).with_context(|| format!("cannot read {}", file.display()))?;
    let mut psbt = parse_psbt(&bytes)?;

    let mut signer = LocalSigner::new(keys::unlock(store)?, network);
    let finalized = signer.sign(&mut psbt)?;

    let out = file.with_extension("signed.psbt");
    fs::write(&out, psbt.to_string())?;
    if finalized {
        ui::ok(&format!("signed  {}", out.display()));
    } else {
        ui::ok(&format!("partially signed  {}", out.display()));
        ui::dim("transaction is not final — other signers are still required");
    }
    Ok(())
}

fn parse_psbt(bytes: &[u8]) -> Result<Psbt> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        if let Ok(psbt) = Psbt::from_str(text.trim()) {
            return Ok(psbt);
        }
    }
    Psbt::deserialize(bytes).map_err(|e| anyhow!("not a valid PSBT: {e}"))
}
