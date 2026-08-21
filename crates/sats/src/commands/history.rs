//! The wallet's transaction history, from the canonical chain view:
//! unconfirmed first, then newest confirmations down.

use std::cmp::Ordering;
use std::collections::HashMap;

use anyhow::Result;
use bdk_wallet::chain::ChainPosition;
use sats_core::bitcoin::Network;

use crate::commands::status::format_signed;
use crate::provider::Services;
use crate::store::{Store, unix_now};
use crate::{ui, walletd};

pub fn run(
    store: &Store,
    network: Network,
    services: &Services,
    offline: bool,
    json: bool,
) -> Result<()> {
    let mut ctx = walletd::open(store, network)?;
    if !offline {
        if let Err(err) = services.sync_wallet(&mut ctx) {
            eprintln!("✗ sync failed — history may be stale ({err:#})");
        }
    }

    // Recipient addresses live in our records, not the chain: enrich rows
    // for transactions this wallet sent.
    let recipients: HashMap<String, String> = store
        .list_transactions(ctx.net_name)?
        .into_iter()
        .map(|r| (r.txid, r.recipient))
        .collect();

    let tip = ctx.wallet.latest_checkpoint().height();
    let txs = ctx.wallet.transactions_sort_by(|a, b| {
        let height =
            |tx: &bdk_wallet::WalletTx| tx.chain_position.confirmation_height_upper_bound();
        let seen = |tx: &bdk_wallet::WalletTx| match tx.chain_position {
            ChainPosition::Unconfirmed {
                first_seen,
                last_seen,
            } => last_seen.or(first_seen),
            ChainPosition::Confirmed { .. } => None,
        };
        match (height(a), height(b)) {
            (None, None) => seen(b).cmp(&seen(a)),
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(ha), Some(hb)) => hb.cmp(&ha),
        }
    });

    struct Row {
        txid: String,
        net_sat: i64,
        fee_sat: Option<u64>,
        height: Option<u32>,
        confirmations: Option<u32>,
        timestamp: Option<u64>,
        recipient: Option<String>,
    }

    let rows: Vec<Row> = txs
        .iter()
        .filter_map(|wtx| {
            let txid = wtx.tx_node.txid;
            let details = ctx.wallet.tx_details(txid)?;
            let (height, timestamp) = match &details.chain_position {
                ChainPosition::Confirmed { anchor, .. } => {
                    (Some(anchor.block_id.height), Some(anchor.confirmation_time))
                }
                ChainPosition::Unconfirmed {
                    first_seen,
                    last_seen,
                } => (None, first_seen.or(*last_seen)),
            };
            let txid = txid.to_string();
            Some(Row {
                net_sat: details.balance_delta.to_sat(),
                fee_sat: details.fee.map(|f| f.to_sat()),
                height,
                confirmations: height.map(|h| tip.saturating_sub(h) + 1),
                timestamp,
                recipient: recipients.get(&txid).cloned(),
                txid,
            })
        })
        .collect();

    if json {
        let list: Vec<_> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "txid": r.txid,
                    "direction": direction(r.net_sat),
                    "net_sat": r.net_sat,
                    "fee_sat": r.fee_sat,
                    "status": if r.height.is_some() { "confirmed" } else { "unconfirmed" },
                    "height": r.height,
                    "confirmations": r.confirmations,
                    "timestamp": r.timestamp,
                    "recipient": r.recipient,
                })
            })
            .collect();
        println!("{}", serde_json::json!(list));
        return Ok(());
    }

    if rows.is_empty() {
        ui::dim("no transactions");
        return Ok(());
    }

    let now = unix_now();
    let header = ["Txid", "Dir", "Net", "Status", "Age", "To"];
    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|r| {
            [
                r.txid[..8].to_string(),
                direction(r.net_sat).to_string(),
                format_signed(r.net_sat),
                match r.confirmations {
                    Some(c) => format!("{c} conf"),
                    None => "mempool".into(),
                },
                r.timestamp
                    .map(|t| ui::human_duration(now.saturating_sub(t)))
                    .unwrap_or_else(|| "—".into()),
                r.recipient.clone().unwrap_or_else(|| "—".into()),
            ]
        })
        .collect();

    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in &cells {
        for (w, cell) in widths.iter_mut().zip(row.iter()) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: &[String]| {
        cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<width$}", width = w))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    ui::dim(&line(&header.map(String::from)));
    for row in &cells {
        println!("{}", line(row));
    }
    Ok(())
}

fn direction(net_sat: i64) -> &'static str {
    match net_sat.cmp(&0) {
        Ordering::Less => "sent",
        Ordering::Greater => "received",
        Ordering::Equal => "self",
    }
}
