//! The human review queue: agent send requests and their outcomes.

use std::collections::BTreeSet;

use anyhow::Result;
use sats_core::bitcoin::Network;
use sats_core::fmt::format_sats;
use sats_core::request::{AgentRequest, RequestOutcome};

use crate::config::network_name;
use crate::store::{Store, unix_now};
use crate::ui;

/// How often `--watch` re-reads the store.
const WATCH_POLL: std::time::Duration = std::time::Duration::from_millis(1_000);

pub fn run(store: &Store, network: Network, all: bool, watch: bool, json: bool) -> Result<()> {
    if watch {
        return watch_loop(store, network, json);
    }
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

/// The trusted discovery channel: poll the local store and print each
/// request once, when it newly awaits an approval. Read-only by
/// construction — no provider, no grant writes, no events, no strikes —
/// so the human learns about pending asks from sats itself rather than
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
            if !awaits_approval(&request, now) {
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
        // queue — say its approval expired unconsumed — announces again.
        announced = awaiting;
        std::thread::sleep(WATCH_POLL);
    }
}

/// Awaiting a human approval right now: an undismissed, approvable
/// denial with no valid approval already armed. Hard denials are not
/// "awaiting" anything — approval cannot move them — and stay visible
/// in `sats agent requests --all` instead.
fn awaits_approval(request: &AgentRequest, now: u64) -> bool {
    let Some(RequestOutcome::Denied { deny, .. }) = &request.outcome else {
        return false;
    };
    if !deny.approvable() || request.dismissed_at.is_some() {
        return false;
    }
    !request
        .approval
        .as_ref()
        .is_some_and(|approval| approval.is_valid_for(&request.intent_digest, now))
}

/// One line per newly pending request: what, from whom, why, and the
/// exact command that approves it.
fn watch_line(request: &AgentRequest, now: u64) -> String {
    let reason = match &request.outcome {
        Some(RequestOutcome::Denied { deny, .. }) => deny.code(),
        _ => "pending",
    };
    format!(
        "{age:>10}  {agent}  {amount} sat → {recipient}  {reason}  approve: sats agent approve {id}",
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
