//! One-time approval of an exact denied agent request.
//!
//! The approval binds the request's intent digest — network, agent,
//! recipient, amount — so it authorizes exactly the send the human saw,
//! once, under an explicit fee ceiling, until it expires or is revoked
//! with `sats agent deny`.

use anyhow::{Context, Result, bail};
use sats_core::authz::{DenyReason, IntentApproval};
use sats_core::bitcoin::Network;
use sats_core::fmt::format_sats;
use sats_core::request::RequestOutcome;

use crate::config::network_name;
use crate::store::{Store, unix_now};
use crate::{keys, ui};

pub fn run(
    store: &Store,
    network: Network,
    id_or_prefix: &str,
    max_fee: Option<u64>,
    duration: &str,
    json: bool,
) -> Result<()> {
    let net_name = network_name(network);
    let request = store.find_agent_request(net_name, id_or_prefix)?;
    if matches!(request.outcome, Some(RequestOutcome::Sent { .. })) {
        bail!(
            "request {} was already sent — nothing to approve",
            request.id
        );
    }
    match store.claim_agent_request(net_name, &request.agent, &request.id)? {
        Some(claim) => drop(claim),
        None => bail!(
            "request {} is executing right now — wait for it to settle",
            request.id
        ),
    }

    let lifetime = humantime::parse_duration(duration)
        .with_context(|| format!("invalid --for {duration:?} (try 1h, 30m)"))?
        .as_secs();
    if lifetime == 0 {
        bail!("--for must be a positive duration");
    }

    // The ceiling defaults to twice the fee the denial recorded; a denial
    // that never learned a real fee (the offline precheck) needs an
    // explicit choice.
    let recorded_fee = denied_fee(&request.outcome, request.amount_sat);
    let max_fee_sat = match (max_fee, recorded_fee) {
        (Some(explicit), _) => explicit,
        (None, Some(fee)) => fee.saturating_mul(2),
        (None, None) => bail!(
            "the denial did not record a fee estimate — pass --max-fee <SATS> to set the ceiling"
        ),
    };

    if !json {
        let mut rows = vec![
            ("Approve", request.id.clone()),
            ("Agent", request.agent.clone()),
            ("Recipient", request.recipient.clone()),
            ("Amount", format!("{} sat", format_sats(request.amount_sat))),
        ];
        if let Some(RequestOutcome::Denied { deny, .. }) = &request.outcome {
            rows.push(("Denied", deny.code().to_string()));
        }
        rows.push(("Max fee", format!("{} sat", format_sats(max_fee_sat))));
        rows.push(("For", ui::human_duration(lifetime)));
        ui::kv_rows(&rows);
        if network == Network::Bitcoin {
            ui::warn("mainnet approval — this authorizes real bitcoin, once");
        }
    }

    // The password prompt IS the human authorization, exactly as it is
    // for grant creation; the unlocked mnemonic itself is not needed.
    let _ = keys::unlock(store)?;

    let now = unix_now();
    let replaced;
    let approval = IntentApproval {
        intent_digest: request.intent_digest.clone(),
        approved_at: now,
        expires_at: now.saturating_add(lifetime),
        max_fee_sat,
        consumed_at: None,
        consumed_by_request: None,
    };
    {
        // Under the grant lock: approval writes serialize with budget
        // decisions and with a concurrent deny.
        let _lock = store.lock_grants(net_name)?;
        let mut fresh = store
            .load_agent_request(net_name, &request.agent, &request.id)?
            .with_context(|| format!("request {} disappeared", request.id))?;
        if matches!(fresh.outcome, Some(RequestOutcome::Sent { .. })) {
            bail!("request {} was already sent — nothing to approve", fresh.id);
        }
        replaced = fresh
            .approval
            .as_ref()
            .is_some_and(|a| a.consumed_at.is_none());
        fresh.approval = Some(approval.clone());
        // An explicit approval outranks an earlier dismissal.
        fresh.dismissed_at = None;
        fresh.updated_at = now;
        store.save_agent_request(net_name, &fresh)?;
    }
    if let Err(err) = store.append_event(
        net_name,
        &sats_core::event::AgentEvent {
            format_version: sats_core::event::EVENT_FORMAT_VERSION,
            at: now,
            network: net_name.to_string(),
            agent: request.agent.clone(),
            request_id: request.id.clone(),
            intent_digest: request.intent_digest.clone(),
            kind: sats_core::event::EventKind::Approved {
                max_fee_sat,
                approval_expires_at: approval.expires_at,
            },
        },
    ) {
        eprintln!("⚠ event log append failed: {err:#}");
    }

    let grant_active = store
        .load_grant(net_name, &request.agent)?
        .is_some_and(|g| !g.is_expired(now));

    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": request.id,
                "agent": request.agent,
                "recipient": request.recipient,
                "amount_sat": request.amount_sat,
                "max_fee_sat": max_fee_sat,
                "expires_at": approval.expires_at,
                "replaced": replaced,
            })
        );
    } else {
        println!();
        ui::ok(&format!(
            "approved  {} (single use, expires in {})",
            request.id,
            ui::human_duration(lifetime)
        ));
        ui::dim("the agent's next matching send consumes this approval");
    }
    if !grant_active {
        // In JSON mode stdout carries only the result object.
        let warning = format!(
            "{} holds no active grant — the approval waits until one exists",
            request.agent
        );
        if json {
            eprintln!("⚠ {warning}");
        } else {
            ui::warn(&warning);
        }
    }
    Ok(())
}

/// The fee the recorded denial actually saw, when it saw one.
fn denied_fee(outcome: &Option<RequestOutcome>, amount_sat: u64) -> Option<u64> {
    let Some(RequestOutcome::Denied { deny, .. }) = outcome else {
        return None;
    };
    match deny {
        DenyReason::OverMaxFee { fee_sat, .. } => Some(*fee_sat),
        DenyReason::ApprovalFeeExceeded { fee_sat, .. } => Some(*fee_sat),
        // Budget denials record amount + fee; the precheck's fee is 0 and
        // carries no signal.
        DenyReason::OverBudget { requested_sat, .. } => {
            requested_sat.checked_sub(amount_sat).filter(|fee| *fee > 0)
        }
        DenyReason::OverMaxTx { .. }
        | DenyReason::Expired { .. }
        | DenyReason::IntentNotGranted { .. }
        | DenyReason::AmountOverflow { .. }
        | DenyReason::AskRequired
        | DenyReason::OverAskMax { .. }
        | DenyReason::ObserveOnly
        | DenyReason::Suspended { .. }
        | DenyReason::RecipientNotAllowed { .. } => None,
    }
}
