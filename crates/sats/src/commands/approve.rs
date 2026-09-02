//! `sats agent approve` — the human-authorized execution path, v0.0.1.
//!
//! A thin adapter: the request is staged on current chain state so the
//! human reviews the real fee, the password prompt is the authorization,
//! and the shared executor in `crate::request::execute` does the rest.

use anyhow::Result;
use sats_core::bitcoin::Network;
use sats_core::fmt::format_sats;
use sats_core::signer::LocalSigner;

use crate::provider::Services;
use crate::request::execute::{self, Outcome, Stage};
use crate::store::Store;
use crate::{keys, ui};

pub fn run(
    store: &Store,
    network: Network,
    services: &Services,
    id_or_prefix: &str,
    yes: bool,
    json: bool,
) -> Result<()> {
    let staged = match execute::stage(store, network, services, id_or_prefix)? {
        Stage::Ready(staged) => staged,
        Stage::Denied(request, reason) => return report_denied(&request.id, &reason, json),
    };

    // The human reviews the real transaction before authorizing it, in
    // every mode. With --json, stdout stays a machine-readable result and
    // the review goes to stderr, the human's channel.
    let rows = vec![
        ("Approve", staged.request.id.clone()),
        ("Agent", staged.request.agent.clone()),
        ("Recipient", staged.request.recipient.clone()),
        (
            "Amount",
            format!("{} sat", format_sats(staged.spend.amount_sat)),
        ),
        ("Fee", format!("{} sat", format_sats(staged.spend.fee_sat))),
        (
            "Total",
            format!("{} sat", format_sats(staged.spend.total_sat())),
        ),
        (
            "Budget",
            format!(
                "{} sat remaining after this",
                format_sats(
                    staged
                        .grant
                        .remaining_sat()
                        .saturating_sub(staged.spend.total_sat())
                )
            ),
        ),
    ];
    if json {
        ui::kv_rows_stderr(&rows);
        if network == Network::Bitcoin {
            eprintln!("! mainnet approval — this signs and broadcasts real bitcoin");
        }
        if !yes && !ui::confirm_stderr("Approve and sign?", true)? {
            eprintln!("aborted — the request stays pending");
            return Ok(());
        }
    } else {
        ui::kv_rows(&rows);
        if network == Network::Bitcoin {
            ui::warn("mainnet approval — this signs and broadcasts real bitcoin");
        }
        if !yes && !ui::confirm("Approve and sign?", true)? {
            ui::dim("aborted — the request stays pending");
            return Ok(());
        }
    }

    // The password prompt IS the human authorization. The mnemonic it
    // unseals is held by this process from here until the signer is
    // dropped: captured by the factory closure, which the executor
    // invokes only after the reservation is durable, and zeroized with
    // the signer. Nothing derived from it is written anywhere.
    let mnemonic = keys::unlock(store)?;
    let request_id = staged.request.id.clone();
    let outcome = execute::commit(store, network, services, *staged, move || {
        Ok(Box::new(LocalSigner::new(mnemonic, network)))
    })?;

    match outcome {
        Outcome::Sent {
            txid,
            amount_sat,
            fee_sat,
            remaining_sat,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "id": request_id,
                        "status": "sent",
                        "txid": txid,
                        "amount_sat": amount_sat,
                        "fee_sat": fee_sat,
                        "total_sat": amount_sat.saturating_add(fee_sat),
                        "remaining_budget_sat": remaining_sat,
                    })
                );
            } else {
                println!();
                ui::ok(&format!("sent  {request_id}  {txid}"));
            }
            Ok(())
        }
        Outcome::BroadcastPending {
            txid,
            amount_sat,
            fee_sat,
            message,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "id": request_id,
                        "status": "broadcast_pending",
                        "txid": txid,
                        "amount_sat": amount_sat,
                        "fee_sat": fee_sat,
                        "message": message,
                    })
                );
                Ok(())
            } else {
                anyhow::bail!("{message}")
            }
        }
        Outcome::Unresolved { txid, message } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "id": request_id,
                        "status": "unresolved",
                        "txid": txid,
                        "message": message,
                    })
                );
                Ok(())
            } else {
                anyhow::bail!(
                    "{message} — the request is unresolved: sats will not sign it again or \
                     refund it; check sats status, then dismiss it"
                )
            }
        }
        Outcome::Failed { message } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "id": request_id,
                        "status": "failed",
                        "message": message,
                    })
                );
                Ok(())
            } else {
                anyhow::bail!("{message} — the request can be approved again")
            }
        }
        Outcome::Denied(reason) => report_denied(&request_id, &reason, json),
    }
}

/// A grant boundary refused the request at execution. In JSON the
/// terminal state is the result; for a human it is a failure to act on.
fn report_denied(
    request_id: &str,
    reason: &sats_core::authz::DenyReason,
    json: bool,
) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": request_id,
                "status": "denied",
                "reason": reason.code(),
            })
        );
        return Ok(());
    }
    anyhow::bail!(
        "denied  {request_id} — {} (outside the grant; only changing the grant lifts it)",
        reason.code()
    )
}
