//! The human review queue: agent requests and their states.

use std::collections::BTreeSet;
use std::io::{IsTerminal, Write};

use anyhow::{Context, Result, bail};
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
        .filter(|request| all || needs_attention(request))
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
            "no agent requests need attention — sats agent requests --all shows settled ones"
        });
        return Ok(());
    }

    let header = [
        "Id",
        "Agent",
        "Recipient",
        "Amount",
        "Status",
        "Age",
        "Next step",
    ];
    let rows: Vec<[String; 7]> = requests
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
                next_step(request),
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
    if !all && requests.iter().any(|r| r.is_approvable()) {
        println!();
        ui::dim("review one:  sats agent approve    dismiss one:  sats agent dismiss <id>");
    }
    Ok(())
}

fn needs_attention(request: &AgentRequest) -> bool {
    request.is_approvable()
        || matches!(
            request.state,
            RequestState::Signing { .. }
                | RequestState::BroadcastPending { .. }
                | RequestState::Unresolved { .. }
        )
}

fn next_step(request: &AgentRequest) -> String {
    match &request.state {
        RequestState::PendingApproval => "review for approval".into(),
        RequestState::Failed { message, .. } => {
            format!("review again; stopped before signing: {message}")
        }
        RequestState::Signing { .. } => "execution active; refresh to observe".into(),
        RequestState::BroadcastPending { txid, .. } => {
            format!("signed; broadcast unconfirmed; sats tx broadcast {txid}")
        }
        RequestState::Unresolved { txid, message, .. } => {
            let target = txid.as_deref().unwrap_or("");
            format!("inspect: sats status {target}; may be signed; never approve again; {message}")
        }
        _ => String::new(),
    }
}

/// The human menu refreshes the existing reconciliation path. Only
/// approvable states receive numbers; recovery rows cannot be selected.
pub fn select_for_review(store: &Store, network: Network) -> Result<Option<AgentRequest>> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "an explicit request id is required when stdin is not a terminal: sats agent approve <id>"
        );
    }
    let mut input = std::io::stdin().lock();
    let mut out = std::io::stderr().lock();
    loop {
        let requests = list_reconciled(store, network)?;
        let eligible = render_selection(&mut out, &requests)?;
        match ui::review_choice(&mut input, &mut out, eligible.len())? {
            ui::ReviewChoice::Select(index) => return Ok(Some(eligible[index].clone())),
            ui::ReviewChoice::Refresh => {}
            ui::ReviewChoice::Wait => std::thread::sleep(WATCH_POLL),
            ui::ReviewChoice::Cancel => {
                writeln!(out, "cancelled — no request authorized")?;
                return Ok(None);
            }
        }
    }
}

fn render_selection<'a>(
    out: &mut impl Write,
    requests: &'a [AgentRequest],
) -> Result<Vec<&'a AgentRequest>> {
    writeln!(
        out,
        "\nAgent requests — select one to review (selection does not authorize)"
    )?;
    let eligible: Vec<_> = requests.iter().filter(|r| r.is_approvable()).collect();
    if eligible.is_empty() {
        writeln!(out, "No requests available for approval.")?;
    }
    for (index, request) in eligible.iter().enumerate() {
        writeln!(
            out,
            "{}. {}  {} sat → {}  [{}]  {}",
            index + 1,
            request.agent,
            format_sats(request.amount_sat),
            request.recipient,
            request.id,
            next_step(request),
        )?;
    }
    for request in requests
        .iter()
        .filter(|r| needs_attention(r) && !r.is_approvable())
    {
        writeln!(
            out,
            "Attention: {}  {}  {}",
            request.agent,
            request.id,
            next_step(request)
        )?;
    }
    Ok(eligible)
}

/// Re-read the exact selected record, failing closed if it disappeared or
/// changed. An old menu index must never select a different request.
pub fn reread_selection(store: &Store, network: Network, selected: &AgentRequest) -> Result<()> {
    let current = store
        .load_agent_request(network_name(network), &selected.agent, &selected.id)?
        .with_context(|| {
            format!(
                "selected request {} disappeared — run sats agent approve to refresh",
                selected.id
            )
        })?;
    validate_selection(selected, &current)
}

pub fn validate_selection(selected: &AgentRequest, current: &AgentRequest) -> Result<()> {
    if !current.version_supported()
        || !current.is_approvable()
        || selected.id != current.id
        || selected.network != current.network
        || selected.agent != current.agent
        || selected.grant_id != current.grant_id
        || selected.idempotency_key != current.idempotency_key
        || selected.recipient != current.recipient
        || selected.amount_sat != current.amount_sat
        || selected.intent_digest != current.intent_digest
        || selected.created_at != current.created_at
        || selected.updated_at != current.updated_at
        || selected.state != current.state
    {
        bail!(
            "selected request {} changed — run sats agent approve to review it again",
            selected.id
        );
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
