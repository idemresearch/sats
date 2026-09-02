//! The human review queue: agent requests and their states.

use std::collections::BTreeSet;

use anyhow::Result;
use sats_core::bitcoin::Network;
use sats_core::fmt::format_sats;
use sats_core::request::{AgentRequest, RequestState};

use crate::config::network_name;
use crate::request::list_reconciled;
use crate::store::{Store, unix_now};
use crate::ui;

/// How often `--watch` re-reads the store.
const WATCH_POLL: std::time::Duration = std::time::Duration::from_millis(1_000);

pub fn run(store: &Store, network: Network, all: bool, watch: bool, json: bool) -> Result<()> {
    if watch {
        return watch_loop(store, network, json);
    }
    let now = unix_now();
    // Listing is a human surface: interrupted executions are settled
    // here, so what the human sees is the durable truth.
    let requests: Vec<AgentRequest> = list_reconciled(store, network)?
        .into_iter()
        .filter(|request| all || request.is_pending_approval())
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
            "no pending agent requests — sats agent requests --all shows settled ones"
        });
        return Ok(());
    }

    let header = ["Id", "Agent", "Recipient", "Amount", "Status", "Age"];
    let rows: Vec<[String; 6]> = requests
        .iter()
        .map(|request| {
            [
                request.id.clone(),
                request.agent.clone(),
                elide(&request.recipient),
                format_sats(request.amount_sat),
                status_cell(request),
                format!(
                    "{} ago",
                    ui::human_duration(now.saturating_sub(request.created_at))
                ),
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
    if !all && requests.iter().any(|r| r.is_pending_approval()) {
        println!();
        ui::dim("approve one:  sats agent approve <id>    dismiss one:  sats agent dismiss <id>");
    }
    Ok(())
}

/// The trusted discovery channel: poll the local store and print each
/// request once, when it newly awaits a decision. Read-only by
/// construction — no provider, no grant writes, no events — so the
/// human learns about pending requests from sats itself rather than
/// from the agent's own retelling. Ctrl-C stops it; nothing is held.
fn watch_loop(store: &Store, network: Network, json: bool) -> Result<()> {
    let net_name = network_name(network);
    if !json {
        eprintln!("watching {net_name} agent requests — Ctrl-C to stop");
    }
    let mut announced: BTreeSet<(String, String)> = BTreeSet::new();
    loop {
        let now = unix_now();
        // A torn or unreadable record is warn-and-skipped inside the
        // listing, exactly like the one-shot view; the stream continues.
        let mut awaiting = BTreeSet::new();
        let mut requests = store.list_agent_requests(net_name)?;
        requests.sort_by_key(|request| request.created_at);
        for request in requests {
            if !request.is_pending_approval() {
                continue;
            }
            // Request ids are unique within an agent, not across agents.
            let key = (request.agent.clone(), request.id.clone());
            let already_announced = announced.contains(&key);
            awaiting.insert(key);
            if already_announced {
                continue;
            }
            if json {
                // JSONL: one full record per line, unelided.
                println!("{}", serde_json::to_string(&request)?);
            } else {
                println!("{}", watch_line(&request, now));
            }
        }
        // Forgetting settled keys means a request that *re-enters* the
        // queue announces again.
        announced = awaiting;
        std::thread::sleep(WATCH_POLL);
    }
}

/// One line per newly pending request: what, from whom, and the exact
/// command that approves it.
fn watch_line(request: &AgentRequest, now: u64) -> String {
    format!(
        "{age:>10}  {agent}  {amount} sat → {recipient}  pending_approval  approve: sats agent approve {id}",
        age = format!(
            "{} ago",
            ui::human_duration(now.saturating_sub(request.created_at))
        ),
        agent = request.agent,
        amount = format_sats(request.amount_sat),
        recipient = elide(&request.recipient),
        id = request.id,
    )
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

fn status_cell(request: &AgentRequest) -> String {
    match &request.state {
        RequestState::Denied { deny, .. } => format!("denied {}", deny.code()),
        RequestState::Sent { txid, .. } => format!("sent {}", &txid[..8.min(txid.len())]),
        RequestState::BroadcastPending { txid, .. } => {
            format!("broadcast_pending {}", &txid[..8.min(txid.len())])
        }
        other => other.status().to_string(),
    }
}
