use anyhow::{Result, bail};
use sats_core::bitcoin::Network;

use crate::cli::SendArgs;
use crate::commands::prepare;
use crate::provider::Services;
use crate::store::{Store, write_atomic};
use crate::{keys, spend, ui, walletd};

pub fn run(
    store: &Store,
    network: Network,
    services: &Services,
    args: &SendArgs,
    json: bool,
) -> Result<()> {
    let mut ctx = walletd::open(store, network)?;
    let req = prepare::PrepareRequest {
        address: &args.address,
        amount: args.amount,
        fee_rate: args.fee_rate,
        allow_dust: args.allow_dust,
        no_guards: args.no_guards,
    };
    let prepared = prepare::build(&mut ctx, services, &req)?;

    if args.dry_run {
        // Nothing is persisted — not even the wallet's revealed change
        // index, so a preview never burns durable state.
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "recipient": prepared.recipient,
                    "amount_sat": prepared.amount_sat,
                    "fee_sat": prepared.fee_sat,
                    "total_sat": prepared.total_sat(),
                    "excluded_utxos": prepared.excluded_utxos,
                    "dry_run": true,
                })
            );
        } else {
            prepare::print_block(&prepared);
            ui::dim("dry run — nothing signed or saved");
        }
        return Ok(());
    }
    ctx.persist()?;

    if let Some(file) = &args.export_psbt {
        // The PSBT names the wallet's UTXOs and change: owner-only perms.
        write_atomic(file, prepared.psbt().to_string().as_bytes(), true)?;
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "recipient": prepared.recipient,
                    "amount_sat": prepared.amount_sat,
                    "fee_sat": prepared.fee_sat,
                    "total_sat": prepared.total_sat(),
                    "excluded_utxos": prepared.excluded_utxos,
                    "psbt_file": file.display().to_string(),
                })
            );
        } else {
            prepare::print_block(&prepared);
            println!();
            ui::ok(&format!("unsigned PSBT written  {}", file.display()));
            ui::dim(&format!("next: sats psbt sign {}", file.display()));
        }
        return Ok(());
    }

    if !json {
        prepare::print_block(&prepared);
    }
    if !args.yes && !ui::confirm("Sign?", true)? {
        ui::dim("aborted");
        return Ok(());
    }

    let mut record = spend::sign_to_record(prepared, keys::unlock(store)?, network, None)?
        .with_origin(sats_core::plan::TxOrigin {
            surface: "cli".into(),
            agent: None,
            request_id: None,
            intent_digest: None,
        });
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
                "{err:#} — transaction {} saved, retry: sats tx broadcast {}",
                record.txid,
                record.txid
            );
        }
    }
}
