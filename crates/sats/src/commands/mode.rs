//! Switch a grant's authority mode: auto, ask, or observe.
//!
//! The attenuation rule, made mechanical: tightening authority
//! (auto→ask→observe) needs no password — reducing what an agent may do
//! stays cheap — while widening requires the wallet password, exactly
//! like issuing the grant did.

use anyhow::{Context, Result, bail};
use sats_core::authz::GrantMode;
use sats_core::bitcoin::Network;
use sats_core::event::{AgentEvent, CONTROL_EVENT_ID, EVENT_FORMAT_VERSION, EventKind};

use crate::config::network_name;
use crate::store::{Store, now_checked, unix_now};
use crate::{keys, ui};

pub fn run(store: &Store, network: Network, agent: &str, mode: &str, json: bool) -> Result<()> {
    let target: GrantMode = mode.parse().map_err(|e: String| anyhow::anyhow!(e))?;
    let net_name = network_name(network);
    let grant = store
        .load_grant(net_name, agent)?
        .with_context(|| format!("no grant for {agent:?} — nothing to change"))?;
    if grant.is_expired(now_checked()?) {
        bail!("grant for {agent:?} has expired — re-issue it instead of changing its mode");
    }
    let from = grant.mode;
    if from == target {
        if json {
            println!(
                "{}",
                serde_json::json!({ "agent": agent, "mode": target.as_str(), "changed": false })
            );
        } else {
            ui::dim(&format!("{agent} is already in {} mode", target.as_str()));
        }
        return Ok(());
    }
    let widening = from.widens_to(target);

    if !json {
        ui::kv_rows(&[
            ("Agent", agent.to_string()),
            ("Mode", format!("{} → {}", from.as_str(), target.as_str())),
            (
                "Direction",
                if widening {
                    "widens authority (password required)".to_string()
                } else {
                    "tightens authority".to_string()
                },
            ),
        ]);
    }
    if widening {
        // The password prompt IS the authorization, exactly as it is for
        // grant creation. Tightening never prompts.
        keys::verify_password(store)?;
    }

    {
        // Under the grant lock; and re-derive the direction from the
        // fresh record, so a concurrent change cannot turn the free
        // tightening path into an unauthorized widening.
        let _lock = store.lock_grants(net_name)?;
        let mut fresh = store
            .load_grant(net_name, agent)?
            .with_context(|| format!("grant for {agent:?} disappeared"))?;
        if fresh.mode == target {
            return Ok(());
        }
        if fresh.mode.widens_to(target) && !widening {
            bail!("the grant changed while this command ran — retry");
        }
        fresh.mode = target;
        store.save_grant(net_name, &fresh)?;
    }
    if let Err(err) = store.append_event(
        net_name,
        &AgentEvent {
            format_version: EVENT_FORMAT_VERSION,
            at: unix_now(),
            network: net_name.to_string(),
            agent: agent.to_string(),
            request_id: CONTROL_EVENT_ID.into(),
            intent_digest: CONTROL_EVENT_ID.into(),
            kind: EventKind::ModeChanged {
                from: from.as_str().into(),
                to: target.as_str().into(),
                widened: widening,
            },
        },
    ) {
        eprintln!("⚠ event log append failed: {err:#}");
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "agent": agent,
                "mode": target.as_str(),
                "previous": from.as_str(),
                "widened": widening,
                "changed": true,
            })
        );
    } else {
        println!();
        ui::ok(&format!("{agent} is now in {} mode", target.as_str()));
        match target {
            GrantMode::Auto => ui::dim("sends inside the caps execute without you"),
            GrantMode::Ask => ui::dim("every send now waits for: sats agent approve"),
            GrantMode::Observe => ui::dim("no send can be authorized or approved"),
        }
    }
    Ok(())
}
