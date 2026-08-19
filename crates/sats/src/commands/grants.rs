use anyhow::Result;
use sats_core::bitcoin::Network;
use sats_core::fmt::format_sats;

use crate::config::network_name;
use crate::store::{Store, unix_now};
use crate::ui;

pub fn run(store: &Store, network: Network, json: bool) -> Result<()> {
    let net_name = network_name(network);
    let now = unix_now();
    let grants = store.active_grants(net_name, now)?;

    if json {
        let list: Vec<_> = grants
            .iter()
            // Never emit wrapped_seed/grant_key on a read surface.
            .map(|g| {
                serde_json::json!({
                    "agent": g.agent,
                    "budget_sat": g.budget_sat,
                    "spent_sat": g.spent_sat,
                    "remaining_sat": g.remaining_sat(),
                    "max_tx_sat": g.max_tx_sat,
                    "max_fee_sat": g.max_fee_sat,
                    "tx_count": g.tx_count,
                    "expires_at": g.expires_at,
                })
            })
            .collect();
        println!("{}", serde_json::json!(list));
        return Ok(());
    }

    if grants.is_empty() {
        ui::dim("no active grants");
        return Ok(());
    }

    let header = ["Agent", "Budget", "Spent", "Remaining", "Max-tx", "Max-fee", "Txs", "Expires"];
    let rows: Vec<[String; 8]> = grants
        .iter()
        .map(|g| {
            [
                g.agent.clone(),
                format_sats(g.budget_sat),
                format_sats(g.spent_sat),
                format_sats(g.remaining_sat()),
                g.max_tx_sat.map(format_sats).unwrap_or_else(|| "—".into()),
                g.max_fee_sat.map(format_sats).unwrap_or_else(|| "—".into()),
                g.tx_count.to_string(),
                format!("in {}", ui::human_duration(g.expires_at.saturating_sub(now))),
            ]
        })
        .collect();

    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in &rows {
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
    for row in &rows {
        println!("{}", line(row));
    }
    Ok(())
}
