//! The human review queue: agent send requests and their outcomes.

use anyhow::Result;
use sats_core::bitcoin::Network;
use sats_core::fmt::format_sats;
use sats_core::request::{AgentRequest, RequestOutcome};

use crate::config::network_name;
use crate::store::{Store, unix_now};
use crate::ui;

pub fn run(store: &Store, network: Network, all: bool, json: bool) -> Result<()> {
    let net_name = network_name(network);
    let now = unix_now();
    let requests: Vec<AgentRequest> = store
        .list_agent_requests(net_name)?
        .into_iter()
        .filter(|request| all || request.is_pending())
        .collect();

    if json {
        // Full records: requests hold payment metadata but no secrets.
        println!("{}", serde_json::to_string(&requests)?);
        return Ok(());
    }
    if requests.is_empty() {
        ui::dim(if all {
            "no agent requests"
        } else {
            "no pending agent requests — sats agent requests --all shows resolved ones"
        });
        return Ok(());
    }

    let header = [
        "Id",
        "Agent",
        "Recipient",
        "Amount",
        "Outcome",
        "Age",
        "Approval",
    ];
    let rows: Vec<[String; 7]> = requests
        .iter()
        .map(|request| {
            [
                request.id.clone(),
                request.agent.clone(),
                elide(&request.recipient),
                format_sats(request.amount_sat),
                outcome_cell(request),
                format!(
                    "{} ago",
                    ui::human_duration(now.saturating_sub(request.created_at))
                ),
                approval_cell(request, now),
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

/// Middle-elide a long address so the table stays scannable; the full
/// value is always in `--json`.
fn elide(address: &str) -> String {
    if address.chars().count() <= 20 {
        return address.to_string();
    }
    let head: String = address.chars().take(10).collect();
    let tail: String = address
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

fn outcome_cell(request: &AgentRequest) -> String {
    match &request.outcome {
        None => "in flight".into(),
        Some(RequestOutcome::Sent { .. }) => "sent".into(),
        Some(RequestOutcome::Denied { deny, .. }) => format!("denied {}", deny.code()),
        Some(RequestOutcome::Failed { txid, .. }) => match txid {
            Some(_) => "failed (signed)".into(),
            None => "failed".into(),
        },
    }
}

fn approval_cell(request: &AgentRequest, now: u64) -> String {
    let Some(approval) = &request.approval else {
        return "—".into();
    };
    if approval.consumed_at.is_some() {
        "consumed".into()
    } else if approval.is_expired(now) {
        "expired".into()
    } else {
        format!(
            "for {}",
            ui::human_duration(approval.expires_at.saturating_sub(now))
        )
    }
}
