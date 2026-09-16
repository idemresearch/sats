//! Where the wallet's transactions stand: signed-but-unbroadcast records
//! waiting for `sats tx broadcast`, and broadcast ones with their
//! confirmation state.

use std::str::FromStr;

use anyhow::Result;
use sats_core::bitcoin::{Network, Txid};
use sats_core::fmt::format_sats;
use sats_core::plan::{TransactionRecord, TransactionStatus};

use crate::provider::Services;
use crate::store::Store;
use crate::walletd::WalletCtx;
use crate::{ui, walletd};

enum ChainState {
    Confirmed { height: u32, confirmations: u32 },
    Mempool,
    Unseen,
}

impl ChainState {
    fn label(&self) -> String {
        match self {
            ChainState::Confirmed { confirmations, .. } => format!("{confirmations} conf"),
            ChainState::Mempool => "in mempool".into(),
            ChainState::Unseen => "not yet seen".into(),
        }
    }

    fn seen(&self) -> &'static str {
        match self {
            ChainState::Confirmed { .. } => "confirmed",
            ChainState::Mempool => "mempool",
            ChainState::Unseen => "unseen",
        }
    }
}

fn chain_state(ctx: &WalletCtx, txid_str: &str) -> ChainState {
    let Ok(txid) = Txid::from_str(txid_str) else {
        return ChainState::Unseen;
    };
    let Some(details) = ctx.wallet.tx_details(txid) else {
        return ChainState::Unseen;
    };
    match details.chain_position.confirmation_height_upper_bound() {
        Some(height) => {
            let tip = ctx.wallet.latest_checkpoint().height();
            ChainState::Confirmed {
                height,
                confirmations: tip.saturating_sub(height) + 1,
            }
        }
        None => ChainState::Mempool,
    }
}

fn entry_json(record: &TransactionRecord, state: &ChainState) -> serde_json::Value {
    let mut entry = serde_json::json!({
        "txid": record.txid,
        "recipient": record.recipient,
        "amount_sat": record.amount_sat,
        "fee_sat": record.fee_sat,
        "total_sat": record.total_sat(),
        "created_at": record.created_at,
        "status": match record.status {
            TransactionStatus::Pending => "pending",
            TransactionStatus::Broadcast => "broadcast",
        },
        "seen": state.seen(),
    });
    if let ChainState::Confirmed {
        height,
        confirmations,
    } = state
    {
        entry["height"] = (*height).into();
        entry["confirmations"] = (*confirmations).into();
    }
    entry
}

pub fn run(
    store: &Store,
    network: Network,
    resolve_services: impl FnOnce() -> Result<Services>,
    txid: Option<&str>,
    offline: bool,
    json: bool,
) -> Result<()> {
    let mut ctx = walletd::open(store, network)?;
    if !offline && let Err(err) = resolve_services()?.sync_wallet(&mut ctx) {
        eprintln!("✗ sync failed — confirmation state may be stale ({err:#})");
    }

    if let Some(id) = txid {
        return detail(store, &ctx, id, json);
    }

    let records = store.list_transactions(ctx.net_name)?;
    let (pending, broadcast): (Vec<_>, Vec<_>) = records
        .into_iter()
        .partition(|r| r.status == TransactionStatus::Pending);

    if json {
        println!(
            "{}",
            serde_json::json!({
                "pending": pending
                    .iter()
                    .map(|r| entry_json(r, &ChainState::Unseen))
                    .collect::<Vec<_>>(),
                "broadcast": broadcast
                    .iter()
                    .map(|r| entry_json(r, &chain_state(&ctx, &r.txid)))
                    .collect::<Vec<_>>(),
            })
        );
        return Ok(());
    }

    if pending.is_empty() && broadcast.is_empty() {
        ui::dim("no transactions — run: sats send");
        return Ok(());
    }
    if !pending.is_empty() {
        ui::dim("pending (signed, not broadcast)");
        for record in &pending {
            println!(
                "{}  {} sat → {}",
                &record.txid[..8],
                format_sats(record.total_sat()),
                record.recipient
            );
            ui::dim(&format!(
                "  broadcast: sats tx broadcast {}",
                &record.txid[..8]
            ));
        }
    }
    if !broadcast.is_empty() {
        if !pending.is_empty() {
            println!();
        }
        ui::dim("broadcast");
        for record in &broadcast {
            println!(
                "{}  {} sat → {}  {}",
                &record.txid[..8],
                format_sats(record.amount_sat),
                record.recipient,
                chain_state(&ctx, &record.txid).label()
            );
        }
    }
    Ok(())
}

fn detail(store: &Store, ctx: &WalletCtx, id: &str, json: bool) -> Result<()> {
    match store.load_transaction(ctx.net_name, id) {
        Ok(record) => {
            let state = match record.status {
                TransactionStatus::Pending => ChainState::Unseen,
                TransactionStatus::Broadcast => chain_state(ctx, &record.txid),
            };
            if json {
                println!("{}", entry_json(&record, &state));
                return Ok(());
            }
            let status = match record.status {
                TransactionStatus::Pending => "pending (signed, not broadcast)".to_string(),
                TransactionStatus::Broadcast => format!("broadcast — {}", state.label()),
            };
            ui::kv_rows(&[
                ("Txid", record.txid.clone()),
                ("Status", status),
                ("To", record.recipient.clone()),
                ("Amount", format!("{} sat", format_sats(record.amount_sat))),
                ("Fee", format!("{} sat", format_sats(record.fee_sat))),
                ("Total", format!("{} sat", format_sats(record.total_sat()))),
            ]);
            if record.status == TransactionStatus::Pending {
                ui::dim(&format!(
                    "broadcast: sats tx broadcast {}",
                    &record.txid[..8]
                ));
            }
            Ok(())
        }
        Err(err) => {
            // Not a saved record; fall back to the wallet's own view of a
            // full txid (e.g. an incoming payment).
            let Ok(txid) = Txid::from_str(id) else {
                return Err(err);
            };
            let Some(details) = ctx.wallet.tx_details(txid) else {
                return Err(err);
            };
            let state = chain_state(ctx, id);
            if json {
                let mut entry = serde_json::json!({
                    "txid": id,
                    "net_sat": details.balance_delta.to_sat(),
                    "fee_sat": details.fee.map(|f| f.to_sat()),
                    "seen": state.seen(),
                });
                if let ChainState::Confirmed {
                    height,
                    confirmations,
                } = &state
                {
                    entry["height"] = (*height).into();
                    entry["confirmations"] = (*confirmations).into();
                }
                println!("{entry}");
            } else {
                ui::kv_rows(&[
                    ("Txid", id.to_string()),
                    ("Status", state.label()),
                    (
                        "Net",
                        format!("{} sat", format_signed(details.balance_delta.to_sat())),
                    ),
                ]);
            }
            Ok(())
        }
    }
}

pub fn format_signed(sat: i64) -> String {
    if sat < 0 {
        format!("-{}", format_sats(sat.unsigned_abs()))
    } else {
        format!("+{}", format_sats(sat.unsigned_abs()))
    }
}
