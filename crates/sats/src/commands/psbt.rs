//! The explicit PSBT escape hatch: inspect and sign file artifacts.
//! Signing consumes only the file the human names — never hidden
//! internal state.

use std::fs;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use sats_core::bitcoin::{Address, Network, Psbt, ScriptBuf};
use sats_core::plan::TransactionRecord;
use sats_core::signer::{LocalSigner, Signer};

use crate::store::{Store, unix_now, write_atomic};
use crate::{keys, ui, walletd};

/// Decode a PSBT file without touching the wallet, the store, or the chain.
pub fn inspect(network: Network, file: &Path, json: bool) -> Result<()> {
    let bytes = fs::read(file).with_context(|| format!("cannot read {}", file.display()))?;
    let psbt = parse_psbt(&bytes)?;

    let txid = psbt.unsigned_tx.compute_txid().to_string();
    let num_inputs = psbt.inputs.len();
    let finalized_inputs = psbt
        .inputs
        .iter()
        .filter(|i| i.final_script_witness.is_some() || i.final_script_sig.is_some())
        .count();
    let signed_inputs = psbt
        .inputs
        .iter()
        .filter(|i| {
            i.final_script_witness.is_some()
                || i.final_script_sig.is_some()
                || i.tap_key_sig.is_some()
                || !i.partial_sigs.is_empty()
        })
        .count();
    let finalized = num_inputs > 0 && finalized_inputs == num_inputs;
    let fee_sat = psbt.fee().ok().map(|a| a.to_sat());
    let outputs: Vec<(String, u64)> = psbt
        .unsigned_tx
        .output
        .iter()
        .map(|o| {
            (
                address_or_script(&o.script_pubkey, network),
                o.value.to_sat(),
            )
        })
        .collect();

    if json {
        println!(
            "{}",
            serde_json::json!({
                "txid": txid,
                "num_inputs": num_inputs,
                "signed_inputs": signed_inputs,
                "finalized": finalized,
                "fee_sat": fee_sat,
                "outputs": outputs
                    .iter()
                    .map(|(address, value_sat)| serde_json::json!({
                        "address": address,
                        "value_sat": value_sat,
                    }))
                    .collect::<Vec<_>>(),
            })
        );
        return Ok(());
    }

    let state = if finalized {
        "finalized".to_string()
    } else if signed_inputs > 0 {
        format!("partially signed ({signed_inputs}/{num_inputs} inputs)")
    } else {
        "unsigned".to_string()
    };
    let mut rows = vec![
        ("Txid", txid),
        ("State", state),
        (
            "Fee",
            fee_sat
                .map(|f| format!("{} sat", sats_core::fmt::format_sats(f)))
                .unwrap_or_else(|| "unknown (inputs carry no UTXO data)".into()),
        ),
    ];
    for (address, value) in &outputs {
        rows.push((
            "Output",
            format!("{} sat → {address}", sats_core::fmt::format_sats(*value)),
        ));
    }
    ui::kv_rows(&rows);
    Ok(())
}

/// Sign a PSBT file. A file that finalizes becomes a pending transaction
/// record ready to broadcast; `--out` keeps everything a file artifact
/// instead.
pub fn sign(
    store: &Store,
    network: Network,
    file: &Path,
    out: Option<&Path>,
    json: bool,
) -> Result<()> {
    let bytes = fs::read(file).with_context(|| format!("cannot read {}", file.display()))?;
    let mut psbt = parse_psbt(&bytes)?;

    let mut signer = LocalSigner::new(keys::unlock(store)?, network);
    let finalized = signer.sign(&mut psbt)?;

    if let Some(out_path) = out {
        write_atomic(out_path, psbt.to_string().as_bytes(), true)?;
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "psbt_file": out_path.display().to_string(),
                    "finalized": finalized,
                })
            );
        } else if finalized {
            ui::ok(&format!("signed  {}", out_path.display()));
            ui::dim("artifact only — nothing saved; sign without --out to stage a broadcast");
        } else {
            ui::ok(&format!("partially signed  {}", out_path.display()));
            ui::dim("transaction is not final — other signers are still required");
        }
        return Ok(());
    }

    if !finalized {
        // Interop path: hand the partially signed PSBT back as a file.
        let out_path = file.with_extension("signed.psbt");
        write_atomic(&out_path, psbt.to_string().as_bytes(), true)?;
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "psbt_file": out_path.display().to_string(),
                    "finalized": false,
                })
            );
        } else {
            ui::ok(&format!("partially signed  {}", out_path.display()));
            ui::dim("transaction is not final — other signers are still required");
        }
        return Ok(());
    }

    // Finalized: persist the raw transaction so broadcast can be retried,
    // deriving display metadata from the wallet's view of the outputs.
    let fee_sat = psbt.fee().map(|a| a.to_sat()).unwrap_or_default();
    let ctx = walletd::open(store, network)?;
    let tx = psbt
        .extract_tx()
        .map_err(|e| anyhow!("cannot extract transaction: {e}"))?;
    let mut amount_sat = 0u64;
    let mut recipient: Option<(String, u64)> = None;
    for output in &tx.output {
        if ctx.wallet.is_mine(output.script_pubkey.clone()) {
            continue;
        }
        let value = output.value.to_sat();
        amount_sat += value;
        if recipient.as_ref().is_none_or(|(_, best)| value > *best) {
            recipient = Some((address_or_script(&output.script_pubkey, network), value));
        }
    }
    let recipient = recipient.map(|(dest, _)| dest).unwrap_or_else(|| {
        // Self-spend: every output is ours; show the first one.
        tx.output
            .first()
            .map(|o| address_or_script(&o.script_pubkey, network))
            .unwrap_or_default()
    });

    let record = TransactionRecord::from_transaction(
        ctx.net_name.to_string(),
        recipient,
        amount_sat,
        fee_sat,
        unix_now(),
        0,
        &tx,
    );
    store.save_transaction(ctx.net_name, &record)?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "txid": record.txid,
                "status": "signed",
                "recipient": record.recipient,
                "amount_sat": record.amount_sat,
                "fee_sat": record.fee_sat,
            })
        );
    } else {
        ui::ok(&format!("signed  {}", record.txid));
        ui::dim(&format!("next: sats tx broadcast {}", record.txid));
    }
    Ok(())
}

fn address_or_script(script: &ScriptBuf, network: Network) -> String {
    Address::from_script(script, network)
        .map(|a| a.to_string())
        .unwrap_or_else(|_| format!("{script:x}"))
}

fn parse_psbt(bytes: &[u8]) -> Result<Psbt> {
    if let Ok(text) = std::str::from_utf8(bytes)
        && let Ok(psbt) = Psbt::from_str(text.trim())
    {
        return Ok(psbt);
    }
    Psbt::deserialize(bytes).map_err(|e| anyhow!("not a valid PSBT: {e}"))
}
