//! The causal event log, rendered for humans.

use anyhow::Result;
use sats_core::bitcoin::Network;
use sats_core::event::{AgentEvent, EventKind};
use sats_core::fmt::format_sats;

use crate::config::network_name;
use crate::store::{Store, unix_now};
use crate::ui;

pub fn run(
    store: &Store,
    network: Network,
    limit: usize,
    request: Option<&str>,
    json: bool,
) -> Result<()> {
    let net_name = network_name(network);
    let mut events = store.list_events(net_name)?;
    if let Some(target) = request {
        let resolved = store.find_agent_request(net_name, target)?;
        events.retain(|event| event.request_id == resolved.id);
    }
    if events.len() > limit {
        events = events.split_off(events.len() - limit);
    }

    if json {
        println!("{}", serde_json::to_string(&events)?);
        return Ok(());
    }
    if events.is_empty() {
        ui::dim("no agent events");
        return Ok(());
    }
    let now = unix_now();
    let kind_w = events.iter().map(|e| e.kind_str().len()).max().unwrap_or(0);
    let id_w = events.iter().map(|e| e.request_id.len()).max().unwrap_or(0);
    for event in &events {
        let age = format!("{} ago", ui::human_duration(now.saturating_sub(event.at)));
        println!(
            "{age:>10}  {kind:<kind_w$}  {id:<id_w$}  {detail}",
            kind = event.kind_str(),
            id = event.request_id,
            detail = detail(event),
        );
    }
    Ok(())
}

fn detail(event: &AgentEvent) -> String {
    match &event.kind {
        EventKind::RequestReceived {
            recipient,
            amount_sat,
        } => format!("{} sat → {recipient}", format_sats(*amount_sat)),
        EventKind::Denied { deny, stage } => format!("{} at {stage}", deny.code()),
        EventKind::Approved {
            max_fee_sat,
            approval_expires_at: _,
        } => format!("max fee {} sat", format_sats(*max_fee_sat)),
        EventKind::ApprovalRevoked => String::new(),
        EventKind::ApprovalConsumed {
            consumed_by_request,
        } => format!("by {consumed_by_request}"),
        EventKind::Reserved {
            total_sat,
            remaining_sat,
            via,
        } => format!(
            "{} sat via {via}, {} sat remaining",
            format_sats(*total_sat),
            format_sats(*remaining_sat)
        ),
        EventKind::Refunded { total_sat } => format!("{} sat", format_sats(*total_sat)),
        EventKind::Signed { txid } | EventKind::Broadcast { txid } => txid.clone(),
        EventKind::BroadcastFailed { txid, message } => format!("{txid}: {message}"),
        EventKind::Failed { message } => message.clone(),
        EventKind::Replayed | EventKind::Conflicted => String::new(),
    }
}
