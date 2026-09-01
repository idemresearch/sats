//! One-time approval of an exact denied agent request.
//!
//! The approval binds the request's intent digest — network, agent,
//! recipient, amount — so it authorizes exactly the send the human saw,
//! once, under an explicit fee ceiling, until it expires or is revoked
//! with `sats agent deny`.

use anyhow::{Context, Result, bail};
use sats_core::authz::{Decision, DenyReason, Grant, IntentApproval, SpendRequest, evaluate_send};
use sats_core::bitcoin::Network;
use sats_core::fmt::format_sats;
use sats_core::request::{AgentRequest, RequestOutcome};

use crate::config::network_name;
use crate::store::{Store, now_checked};
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
    // Any control-plane touch retires a legacy autonomous record from
    // disk before this command reasons about the grant.
    store.normalize_grant_autonomy(net_name, &request.agent)?;
    let grant = ensure_approvable(store, net_name, &request, now_checked()?)?;
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

    // The ceiling defaults to twice the fee the denial recorded. Most
    // denials never saw a real fee (the terminal ask happens at the
    // offline precheck), so the fallback is the grant's own fee cap —
    // the human's standing fee tolerance — which keeps the routine
    // `sats agent approve <id>` working without a flag. Only a grant
    // deliberately issued with no fee cap requires an explicit choice.
    let recorded_fee = denied_fee(&request.outcome, request.amount_sat);
    let max_fee_sat = match (max_fee, recorded_fee, grant.max_fee_sat) {
        (Some(explicit), _, _) => explicit,
        (None, Some(fee), _) => fee.saturating_mul(2),
        (None, None, Some(cap)) => cap,
        (None, None, None) => bail!(
            "the denial did not record a fee estimate and the grant carries no fee cap — \
             pass --max-fee <SATS> to set the ceiling"
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
        rows.push((
            "Budget",
            format!("{} sat remaining", format_sats(grant.remaining_sat())),
        ));
        rows.push(("For", ui::human_duration(lifetime)));
        ui::kv_rows(&rows);
        if request.amount_sat.saturating_add(max_fee_sat) > grant.remaining_sat() {
            ui::warn("this send exceeds the remaining budget — approving overdraws it");
        }
        if network == Network::Bitcoin {
            ui::warn("mainnet approval — this authorizes real bitcoin, once");
        }
    }

    // The password prompt IS the human authorization, exactly as it is
    // for grant creation; the unlocked mnemonic itself is not needed.
    let _ = keys::unlock(store)?;

    let replaced;
    let approval;
    {
        // Under the grant lock: approval writes serialize with budget
        // decisions, policy changes, and a concurrent deny. Recheck after
        // the password prompt so a newly hard restriction cannot arm an
        // approval that the daemon would refuse.
        let _lock = store.lock_grants(net_name)?;
        let now = now_checked()?;
        let mut fresh = store
            .load_agent_request(net_name, &request.agent, &request.id)?
            .with_context(|| format!("request {} disappeared", request.id))?;
        ensure_approvable(store, net_name, &fresh, now)?;
        approval = IntentApproval {
            intent_digest: request.intent_digest.clone(),
            approved_at: now,
            expires_at: now.saturating_add(lifetime),
            max_fee_sat,
            consumed_at: None,
            consumed_by_request: None,
        };
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
            at: approval.approved_at,
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
    Ok(())
}

/// Refuse hard restrictions before prompting, then again under the grant
/// lock before writing; hand back the grant the check ran against. The
/// recorded denial alone can be stale. As in the daemon's precheck, fee
/// zero checks the current amount and hard envelope; the daemon still
/// checks the prepared transaction's actual fee later.
fn ensure_approvable(
    store: &Store,
    network: &str,
    request: &AgentRequest,
    now: u64,
) -> Result<Grant> {
    if matches!(request.outcome, Some(RequestOutcome::Sent { .. })) {
        bail!(
            "request {} was already sent — nothing to approve",
            request.id
        );
    }
    if let Some(RequestOutcome::Denied { deny, .. }) = &request.outcome
        && !deny.approvable()
    {
        bail!(
            "request {} was denied {}, which no approval can lift — the only escalation \
             is changing the grant itself: sats agent grant {} --budget <sats> ...",
            request.id,
            deny.code(),
            request.agent,
        );
    }
    let grant = store
        .load_grant(network, &request.agent)?
        .with_context(|| {
            format!(
                "no active grant for {:?} — issue a grant before approving",
                request.agent
            )
        })?;
    if let Decision::Deny(deny) = evaluate_send(
        &grant,
        &request.recipient,
        &SpendRequest {
            amount_sat: request.amount_sat,
            fee_sat: 0,
        },
        now,
    ) && !deny.approvable()
    {
        bail!(
            "the current grant denies request {} with {}, which no approval can lift — \
             the human must change the grant itself",
            request.id,
            deny.code(),
        );
    }
    Ok(grant)
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
