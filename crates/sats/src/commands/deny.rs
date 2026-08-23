//! Dismiss an agent request and revoke its unconsumed approval.
//!
//! Reducing authority needs no password — the same posture as
//! `sats agent revoke`.

use anyhow::{Context, Result};
use sats_core::bitcoin::Network;

use crate::config::network_name;
use crate::store::{Store, unix_now};
use crate::ui;

pub fn run(store: &Store, network: Network, id_or_prefix: &str, json: bool) -> Result<()> {
    let net_name = network_name(network);
    let request = store.find_agent_request(net_name, id_or_prefix)?;
    let now = unix_now();
    let revoked_approval;
    {
        let _lock = store.lock_grants(net_name)?;
        let mut fresh = store
            .load_agent_request(net_name, &request.agent, &request.id)?
            .with_context(|| format!("request {} disappeared", request.id))?;
        // A consumed approval is history and stays; an armed one dies here.
        revoked_approval = fresh
            .approval
            .as_ref()
            .is_some_and(|a| a.consumed_at.is_none());
        if revoked_approval {
            fresh.approval = None;
        }
        fresh.dismissed_at = Some(now);
        fresh.updated_at = now;
        store.save_agent_request(net_name, &fresh)?;
    }
    if revoked_approval
        && let Err(err) = store.append_event(
            net_name,
            &sats_core::event::AgentEvent {
                format_version: sats_core::event::EVENT_FORMAT_VERSION,
                at: now,
                network: net_name.to_string(),
                agent: request.agent.clone(),
                request_id: request.id.clone(),
                intent_digest: request.intent_digest.clone(),
                kind: sats_core::event::EventKind::ApprovalRevoked,
            },
        )
    {
        eprintln!("⚠ event log append failed: {err:#}");
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": request.id,
                "dismissed": true,
                "approval_revoked": revoked_approval,
            })
        );
    } else if revoked_approval {
        ui::ok(&format!("dismissed  {} (approval revoked)", request.id));
    } else {
        ui::ok(&format!("dismissed  {}", request.id));
    }
    Ok(())
}
