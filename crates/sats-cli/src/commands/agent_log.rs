//! The causal event log, rendered for humans.

use anyhow::Result;
use sats_core::bitcoin::Network;
use sats_core::event::{AgentEvent, EventKind};
use sats_core::fmt::format_sats;
use sats_wallet::config::network_name;
use sats_wallet::store::{EventLine, Store, unix_now};

use crate::ui;

pub fn run(
    store: &Store,
    network: Network,
    limit: usize,
    request: Option<&str>,
    json: bool,
) -> Result<()> {
    let net_name = network_name(network);
    let mut lines = store.list_event_lines(net_name)?;
    if let Some(target) = request {
        let resolved = store.find_agent_request(net_name, target)?;
        lines.retain(|line| request_id(line) == Some(resolved.id.as_str()));
    }
    if lines.len() > limit {
        lines = lines.split_off(lines.len() - limit);
    }
    let unknown = lines
        .iter()
        .filter(|line| matches!(line, EventLine::Unknown(_)))
        .count();

    if json {
        // Unknown lines pass through raw: the log is the audit, and a
        // kind from a newer sats is still part of it.
        let values: Vec<serde_json::Value> = lines
            .iter()
            .map(|line| match line {
                EventLine::Event(event) => serde_json::to_value(event).unwrap_or_default(),
                EventLine::Unknown(value) => value.clone(),
            })
            .collect();
        println!("{}", serde_json::to_string(&values)?);
        return Ok(());
    }
    if lines.is_empty() {
        ui::dim("no agent events");
        return Ok(());
    }
    let now = unix_now();
    let kind_w = lines.iter().map(|l| kind_str(l).len()).max().unwrap_or(0);
    let id_w = lines
        .iter()
        .map(|l| request_id(l).unwrap_or("").len())
        .max()
        .unwrap_or(0);
    for line in &lines {
        let age = format!("{} ago", ui::human_duration(now.saturating_sub(at(line))));
        println!(
            "{age:>10}  {kind:<kind_w$}  {id:<id_w$}  {detail}",
            kind = kind_str(line),
            id = request_id(line).unwrap_or(""),
            detail = detail_line(line),
        );
    }
    if unknown > 0 {
        ui::warn(&format!(
            "{unknown} event{} written by a newer sats shown raw — upgrade sats to interpret {}",
            if unknown == 1 { "" } else { "s" },
            if unknown == 1 { "it" } else { "them" },
        ));
    }
    Ok(())
}

fn request_id(line: &EventLine) -> Option<&str> {
    match line {
        EventLine::Event(event) => Some(event.request_id.as_str()),
        EventLine::Unknown(value) => value.get("request_id").and_then(|v| v.as_str()),
    }
}

fn at(line: &EventLine) -> u64 {
    match line {
        EventLine::Event(event) => event.at,
        EventLine::Unknown(value) => value.get("at").and_then(|v| v.as_u64()).unwrap_or(0),
    }
}

fn kind_str(line: &EventLine) -> &str {
    match line {
        EventLine::Event(event) => event.kind_str(),
        EventLine::Unknown(value) => value
            .get("event")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown"),
    }
}

fn detail_line(line: &EventLine) -> String {
    match line {
        EventLine::Event(event) => detail(event),
        // The raw line is the only honest rendering of a kind this build
        // does not know.
        EventLine::Unknown(value) => format!("(unknown to this sats) {value}"),
    }
}

fn detail(event: &AgentEvent) -> String {
    match &event.kind {
        EventKind::RequestReceived {
            recipient,
            amount_sat,
        } => format!("{} sat → {recipient}", format_sats(*amount_sat)),
        EventKind::Denied { deny, stage } => format!("{} at {stage}", deny.code()),
        EventKind::Approved | EventKind::Dismissed => String::new(),
        EventKind::Reserved {
            total_sat,
            remaining_sat,
        } => format!(
            "{} sat, {} sat remaining",
            format_sats(*total_sat),
            format_sats(*remaining_sat)
        ),
        EventKind::Refunded { total_sat } => format!("{} sat", format_sats(*total_sat)),
        EventKind::Signed { txid } | EventKind::Broadcast { txid } => txid.clone(),
        EventKind::BroadcastFailed { txid, message } => format!("{txid}: {message}"),
        EventKind::Failed { message } => message.clone(),
        EventKind::Unresolved { message, txid } => match txid {
            Some(txid) => format!("{txid}: {message}"),
            None => message.clone(),
        },
        EventKind::Conflicted => String::new(),
        EventKind::ModeChanged { from, to, widened } => {
            format!("{from} → {to}{}", if *widened { " (widened)" } else { "" })
        }
        EventKind::RecipientAllowed { recipient } => format!("+ {recipient}"),
        EventKind::RecipientDisallowed { recipient } => format!("- {recipient}"),
    }
}
