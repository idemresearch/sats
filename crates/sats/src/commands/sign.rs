use std::fs;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use sats_core::bitcoin::{Network, Psbt};
use sats_core::plan::LegacyPlanStatus;
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
    enum Source {
        Session,
        Legacy,
    }

    let (source_id, prepared, source) = match plan_id {
        Some(id) => {
            let session_path = store.psbt_sessions_dir(net_name).join(format!("{id}.json"));
            if session_path.exists() {
                let session = store.load_psbt_session(net_name, &id)?;
                let source_id = session.id.clone();
                (source_id, session.into_prepared()?, Source::Session)
            } else {
                let legacy = store.load_legacy_plan(net_name, &id)?;
                match legacy.status {
                    LegacyPlanStatus::Unsigned => {}
                    LegacyPlanStatus::Signed => {
                        bail!("plan {} is already signed — run: sats broadcast", legacy.id)
                    }
                    LegacyPlanStatus::Broadcast => {
                        bail!("plan {} was already broadcast", legacy.id)
                    }
                }
                let source_id = legacy.id.clone();
                (source_id, legacy.into_prepared()?, Source::Legacy)
            }
        }
        None => {
            if let Some(session) = store.latest_psbt_session(net_name)? {
                let source_id = session.id.clone();
                (source_id, session.into_prepared()?, Source::Session)
            } else {
                let legacy = store
                    .latest_legacy_plan(net_name, LegacyPlanStatus::Unsigned)?
                    .context("no unsigned PSBT sessions — run: sats plan")?;
                let source_id = legacy.id.clone();
                (source_id, legacy.into_prepared()?, Source::Legacy)
            }
        }
    };

    let record = crate::spend::sign_to_record(
        prepared,
        keys::unlock(store)?,
        network,
        Some(source_id.clone()),
    )?;
    store.save_transaction(net_name, &record)?;
    match source {
        Source::Session => store.delete_psbt_session(net_name, &source_id)?,
        Source::Legacy => store.delete_legacy_plan(net_name, &source_id)?,
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": source_id,
                "txid": record.txid,
                "status": "signed",
            })
        );
    } else {
        ui::ok(&format!("signed  {}", record.txid));
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
