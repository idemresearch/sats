use anyhow::{Result, bail};
use sats_core::bitcoin::Network;
use sats_core::plan::PreparedSpend;
use sats_wallet::prepare;
use sats_wallet::provider::Services;
use sats_wallet::store::{Store, write_artifact};
use sats_wallet::{spend, walletd};

use crate::cli::SendArgs;
use crate::{keys, ui};

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
            print_block(&prepared);
            ui::dim("dry run — nothing signed or saved");
        }
        return Ok(());
    }
    ctx.persist()?;

    if let Some(file) = &args.export_psbt {
        // The PSBT names the wallet's UTXOs and change: owner-only perms.
        write_artifact(file, prepared.psbt().to_string().as_bytes())?;
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
            print_block(&prepared);
            println!();
            ui::ok(&format!("unsigned PSBT written  {}", file.display()));
            ui::dim(&format!("next: sats psbt sign {}", file.display()));
        }
        return Ok(());
    }

    if !json {
        print_block(&prepared);
    }
    if !args.yes && !ui::confirm("Sign?", true)? {
        ui::dim("aborted");
        return Ok(());
    }

    let mut record = spend::sign_to_record(prepared, keys::unlock(store)?, network)?.with_origin(
        sats_core::plan::TxOrigin {
            surface: "cli".into(),
            agent: None,
            request_id: None,
            intent_digest: None,
        },
    );
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

fn print_block(plan: &PreparedSpend) {
    ui::sat_rows(&[
        ("Send", plan.amount_sat),
        ("Fee", plan.fee_sat),
        ("Total", plan.total_sat()),
    ]);
}
