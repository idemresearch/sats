//! Agent requests as the native workflow object.
//!
//! Agents create requests. Humans authorize requests. sats executes
//! requests. This module is the reusable layer every surface calls:
//! [`create`] for the agent side (MCP today), [`dismiss`] and
//! [`execute`] for the human-authorized side (the CLI today; any trusted
//! approval surface tomorrow), and [`reconcile`] for interrupted
//! executions. Commands and tools are thin adapters over it.
//!
//! Every read-then-write of grant or request state happens under the
//! grant lock, so grant issuance and revocation, request creation,
//! authorization, outcomes, and budget decisions serialize as one
//! history. A request is bound to the grant instance (`token_id`) that
//! created it and never executes under another.

pub mod execute;

use std::str::FromStr;

use anyhow::{Context, Result, bail};
use sats_core::authz::{
    AGENT_NAME_RULE, Decision, DenyReason, Grant, SpendRequest, evaluate_send, valid_agent_name,
};
use sats_core::bitcoin::{Address, Network};
use sats_core::event::{AgentEvent, EVENT_FORMAT_VERSION, EventKind};
use sats_core::intent::SendIntent;
use sats_core::plan::{TransactionRecord, TransactionStatus};
use sats_core::request::{AgentRequest, REQUEST_FORMAT_VERSION, RequestState};

use crate::config::network_name;
use crate::store::{Store, now_checked, unix_now};

/// The canonical rule for a client-supplied request key.
pub const REQUEST_KEY_RULE: &str = "request_id must be 1-64 characters of A-Za-z0-9_-";

pub fn valid_request_key(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// What an agent asks for. The token authenticates the agent against
/// its grant; nothing is written for a caller that cannot authenticate.
pub struct CreateParams<'a> {
    pub agent: &'a str,
    pub token: &'a str,
    pub client_request_id: Option<&'a str>,
    pub address: &'a str,
    pub amount_sat: u64,
}

/// Why a request could not be created. Operational conditions with a
/// stable code; a policy refusal is not an error — it is a `denied`
/// request the agent can observe.
#[derive(Debug)]
pub enum CreateError {
    InvalidAgent,
    InvalidRequestId,
    InvalidAddress(String),
    ClockUnavailable(String),
    /// No grant on file for this agent: nothing can be authenticated,
    /// so nothing is written.
    NoGrant,
    /// The presented token does not match the grant on file.
    Unauthorized,
    /// The key was already used for a different intent.
    Conflict {
        request_id: String,
    },
    Store(anyhow::Error),
}

impl CreateError {
    pub fn code(&self) -> &'static str {
        match self {
            CreateError::InvalidAgent => "invalid_agent",
            CreateError::InvalidRequestId => "invalid_request_id",
            CreateError::InvalidAddress(_) => "invalid_address",
            CreateError::ClockUnavailable(_) => "clock_unavailable",
            CreateError::NoGrant => "no_grant",
            CreateError::Unauthorized => "unauthorized",
            CreateError::Conflict { .. } => "request_id_conflict",
            CreateError::Store(_) => "store_error",
        }
    }

    pub fn message(&self, agent: &str) -> String {
        match self {
            CreateError::InvalidAgent => AGENT_NAME_RULE.to_string(),
            CreateError::InvalidRequestId => REQUEST_KEY_RULE.to_string(),
            CreateError::InvalidAddress(message) => message.clone(),
            CreateError::ClockUnavailable(message) => message.clone(),
            CreateError::NoGrant => format!(
                "no active grant — ask the human to run: sats agent grant {agent} --budget <sats>"
            ),
            CreateError::Unauthorized => format!(
                "the presented token does not authorize agent {agent:?} — a grant issued or \
                 replaced later has a different token"
            ),
            CreateError::Conflict { request_id } => format!(
                "request_id was already used for a different send ({request_id}) — pick a fresh id"
            ),
            CreateError::Store(err) => format!("{err:#}"),
        }
    }
}

impl From<anyhow::Error> for CreateError {
    fn from(err: anyhow::Error) -> Self {
        CreateError::Store(err)
    }
}

/// Create a request, or return the existing one for a repeated key.
///
/// The grant is read, the token checked, and the record written under
/// the grant lock, so a revoke or re-issue cannot interleave: a request
/// is created under exactly one grant instance and records its
/// `token_id`. The grant's ladder runs at creation with the fee unknown
/// (zero): a proposal inside every boundary is recorded as
/// `pending_approval`; a hard boundary is recorded as `denied`. The
/// audit line is appended *before* the record is written, so no request
/// can exist on disk without its causal event. The same client key with
/// the same canonical intent returns the existing record without
/// writing anything; the same key with a different intent is a conflict.
pub fn create(
    store: &Store,
    network: Network,
    params: &CreateParams,
) -> Result<AgentRequest, CreateError> {
    let net_name = network_name(network);
    // The agent name becomes a path component in every store lookup, so
    // it is gated before anything touches the disk.
    if !valid_agent_name(params.agent) {
        return Err(CreateError::InvalidAgent);
    }
    if params
        .client_request_id
        .is_some_and(|key| !valid_request_key(key))
    {
        return Err(CreateError::InvalidRequestId);
    }
    // Normalize the recipient first: the canonical intent hashes one
    // spelling of the address, so textual variants deduplicate.
    let recipient = Address::from_str(params.address)
        .map_err(|e| CreateError::InvalidAddress(format!("invalid address: {e}")))
        .and_then(|a| {
            a.require_network(network).map_err(|_| {
                CreateError::InvalidAddress(format!("address is not valid for {net_name}"))
            })
        })?
        .to_string();
    // Authorization time fails closed: a broken clock must not un-expire
    // every grant by reporting 1970.
    let now = now_checked().map_err(|e| CreateError::ClockUnavailable(format!("{e:#}")))?;

    let _lock = store.lock_grants(net_name)?;
    let grant = store
        .load_grant(net_name, params.agent)?
        .ok_or(CreateError::NoGrant)?;
    if !grant.authorizes(params.token) {
        return Err(CreateError::Unauthorized);
    }

    let digest = SendIntent {
        network: net_name.to_string(),
        agent: params.agent.to_string(),
        recipient: recipient.clone(),
        amount_sat: params.amount_sat,
    }
    .digest();

    // A repeated key answers from the record: same intent, same request.
    if let Some(key) = params.client_request_id {
        let id = format!("k-{key}");
        if let Some(existing) = store.load_agent_request(net_name, params.agent, &id)? {
            if existing.intent_digest != digest {
                journal_soft(store, net_name, &existing, EventKind::Conflicted);
                return Err(CreateError::Conflict {
                    request_id: existing.id,
                });
            }
            return Ok(existing);
        }
    }

    // The verdict at creation: fee unknown, so amount alone. The full
    // ladder runs — recipient rule included — so a refusal any later
    // stage would repeat is recorded now, before a human sees it.
    let verdict = evaluate_send(
        &grant,
        &recipient,
        &SpendRequest {
            amount_sat: params.amount_sat,
            fee_sat: 0,
        },
        now,
    );
    let state = match &verdict {
        Decision::Ask => RequestState::PendingApproval,
        Decision::Deny(reason) => RequestState::Denied {
            deny: reason.clone(),
            at: now,
        },
    };
    let fresh_record = |id: String| AgentRequest {
        format_version: REQUEST_FORMAT_VERSION,
        id,
        network: net_name.to_string(),
        agent: params.agent.to_string(),
        grant_token_id: grant.token_id.clone(),
        client_request_id: params.client_request_id.map(str::to_string),
        recipient: recipient.clone(),
        amount_sat: params.amount_sat,
        intent_digest: digest.clone(),
        created_at: now,
        updated_at: now,
        state: state.clone(),
    };
    let record = match params.client_request_id {
        Some(key) => fresh_record(format!("k-{key}")),
        None => {
            // Keyless requests get a random id; collisions retry.
            loop {
                let mut bytes = [0u8; 4];
                getrandom::fill(&mut bytes)
                    .map_err(|err| anyhow::anyhow!("cannot generate request id: {err}"))?;
                let candidate = fresh_record(format!("r-{}", hex::encode(bytes)));
                if store
                    .load_agent_request(net_name, params.agent, &candidate.id)?
                    .is_none()
                {
                    break candidate;
                }
            }
        }
    };

    // Audit first, record second: an unwritable audit log refuses the
    // create before anything exists, so a request on disk always has its
    // causal line. (A crash between the two leaves a received line for a
    // request that was never written — honest, and harmless.)
    journal(
        store,
        net_name,
        &record,
        EventKind::RequestReceived {
            recipient: recipient.clone(),
            amount_sat: params.amount_sat,
        },
    )
    .context("cannot record request")?;
    let claim = store
        .create_agent_request(net_name, &record)?
        .context("request appeared between the existence check and the write")?;
    drop(claim);
    if let Decision::Deny(reason) = verdict {
        journal_soft(
            store,
            net_name,
            &record,
            EventKind::Denied {
                deny: reason,
                stage: "create".into(),
            },
        );
    }
    Ok(record)
}

/// A human dismisses a request. Reducing authority needs no password —
/// the same posture as `sats agent revoke`. Dismissing an `unresolved`
/// request closes it without a refund: a signature may exist.
pub fn dismiss(store: &Store, network: Network, id_or_prefix: &str) -> Result<AgentRequest> {
    let net_name = network_name(network);
    let request = store.find_agent_request(net_name, id_or_prefix)?;
    let now = unix_now();
    let dismissed = {
        let _lock = store.lock_grants(net_name)?;
        let mut fresh = store
            .load_agent_request(net_name, &request.agent, &request.id)?
            .with_context(|| format!("request {} disappeared", request.id))?;
        if !fresh.is_dismissable() {
            bail!(
                "request {} is {} — nothing to dismiss",
                fresh.id,
                describe_settled(&fresh)
            );
        }
        fresh.state = RequestState::Dismissed { at: now };
        fresh.updated_at = now;
        store.save_agent_request(net_name, &fresh)?;
        fresh
    };
    journal_soft(store, net_name, &dismissed, EventKind::Dismissed);
    Ok(dismissed)
}

/// The grant a request may execute under: the one on file, only if it is
/// the instance that created the request.
pub(crate) fn bound_grant(
    store: &Store,
    net_name: &str,
    request: &AgentRequest,
) -> Result<Result<Grant, DenyReason>> {
    Ok(match store.load_grant(net_name, &request.agent)? {
        Some(grant) if grant.token_id == request.grant_token_id => Ok(grant),
        _ => Err(DenyReason::Revoked),
    })
}

/// The persisted transaction attributed to exactly this request: same
/// agent, same request id, same canonical intent. Request ids are scoped
/// to their agent, so the id alone identifies nothing.
pub(crate) fn attributed_transaction(
    store: &Store,
    net_name: &str,
    request: &AgentRequest,
) -> Result<Option<TransactionRecord>> {
    Ok(store
        .list_transactions(net_name)?
        .into_iter()
        .find(|record| {
            record.origin.as_ref().is_some_and(|origin| {
                origin.agent.as_deref() == Some(request.agent.as_str())
                    && origin.request_id.as_deref() == Some(request.id.as_str())
                    && origin.intent_digest.as_deref() == Some(request.intent_digest.as_str())
            })
        }))
}

/// Settle an interrupted execution around the signature boundary.
///
/// A request left `signing` by a process that died is resolved from the
/// durable truth only. A persisted transaction attributed to it means a
/// signature exists: the request becomes `broadcast_pending` or `sent` by
/// the record's status. No transaction means the signer may or may not
/// have run: the request becomes `unresolved`, nothing is refunded, and
/// nothing is signed again — a human resolves it. A request whose
/// execution is still live (its claim is held) is left alone.
pub fn reconcile(store: &Store, network: Network, request: AgentRequest) -> Result<AgentRequest> {
    let net_name = network_name(network);
    if !matches!(request.state, RequestState::Signing { .. }) {
        return Ok(request);
    }
    let Some(_claim) = store.claim_agent_request(net_name, &request.agent, &request.id)? else {
        return Ok(request);
    };
    let now = unix_now();
    let _lock = store.lock_grants(net_name)?;
    // Re-read under the lock: the live execution may have settled it in
    // the window before the claim was taken.
    let mut fresh = store
        .load_agent_request(net_name, &request.agent, &request.id)?
        .with_context(|| format!("request {} disappeared", request.id))?;
    if !matches!(fresh.state, RequestState::Signing { .. }) {
        return Ok(fresh);
    }
    match attributed_transaction(store, net_name, &fresh)? {
        Some(record) => {
            // A signature exists: never refund, never sign again.
            fresh.state = match record.status {
                TransactionStatus::Broadcast => RequestState::Sent {
                    txid: record.txid.clone(),
                    fee_sat: record.fee_sat,
                    at: now,
                },
                TransactionStatus::Pending => RequestState::BroadcastPending {
                    txid: record.txid.clone(),
                    fee_sat: record.fee_sat,
                    at: now,
                },
            };
            fresh.updated_at = now;
            store.save_agent_request(net_name, &fresh)?;
            let kind = match record.status {
                TransactionStatus::Broadcast => EventKind::Broadcast {
                    txid: record.txid.clone(),
                },
                TransactionStatus::Pending => EventKind::BroadcastFailed {
                    txid: record.txid.clone(),
                    message: "execution interrupted after signing; retry the broadcast".into(),
                },
            };
            journal_soft(store, net_name, &fresh, kind);
        }
        None => {
            // The signer may have run and nothing durable says how it
            // ended. Neither a refund nor a second signature is safe.
            let message = "execution interrupted at or after signing; a signature may exist — \
                           the budget stays reserved and sats will not sign this request again"
                .to_string();
            fresh.state = RequestState::Unresolved {
                at: now,
                message: message.clone(),
                txid: None,
            };
            fresh.updated_at = now;
            store.save_agent_request(net_name, &fresh)?;
            journal_soft(store, net_name, &fresh, EventKind::Failed { message });
        }
    }
    Ok(fresh)
}

/// Every request for the network, interrupted executions settled first.
pub fn list_reconciled(store: &Store, network: Network) -> Result<Vec<AgentRequest>> {
    let net_name = network_name(network);
    store
        .list_agent_requests(net_name)?
        .into_iter()
        .map(|request| reconcile(store, network, request))
        .collect()
}

/// Settle a `broadcast_pending` request once its transaction has been
/// broadcast by another path (`sats tx broadcast`). The transaction's
/// full attribution — agent, request id, intent digest — must match.
pub fn settle_broadcast(store: &Store, network: Network, txid: &str) -> Result<()> {
    let net_name = network_name(network);
    let Some(record) = store
        .list_transactions(net_name)?
        .into_iter()
        .find(|r| r.txid == txid)
    else {
        return Ok(());
    };
    let Some(origin) = record.origin.as_ref() else {
        return Ok(());
    };
    let (Some(agent), Some(request_id), Some(digest)) = (
        origin.agent.as_deref(),
        origin.request_id.as_deref(),
        origin.intent_digest.as_deref(),
    ) else {
        return Ok(());
    };
    let now = unix_now();
    let _lock = store.lock_grants(net_name)?;
    let Some(mut request) = store.load_agent_request(net_name, agent, request_id)? else {
        return Ok(());
    };
    if request.intent_digest != digest {
        return Ok(());
    }
    let RequestState::BroadcastPending { fee_sat, .. } = request.state else {
        return Ok(());
    };
    request.state = RequestState::Sent {
        txid: txid.to_string(),
        fee_sat,
        at: now,
    };
    request.updated_at = now;
    store.save_agent_request(net_name, &request)?;
    journal_soft(
        store,
        net_name,
        &request,
        EventKind::Broadcast {
            txid: txid.to_string(),
        },
    );
    Ok(())
}

/// A one-line description of why a settled request cannot move.
pub fn describe_settled(request: &AgentRequest) -> String {
    match &request.state {
        RequestState::PendingApproval => "awaiting approval".into(),
        RequestState::Denied { deny, .. } => format!(
            "denied {} — outside the grant; only changing the grant lifts it",
            deny.code()
        ),
        RequestState::Dismissed { .. } => "already dismissed".into(),
        RequestState::Signing { .. } => "signing right now".into(),
        RequestState::Unresolved { txid, .. } => format!(
            "unresolved — a signature may exist{}; sats will not sign it again or refund it; \
             check sats status, then dismiss it",
            txid.as_ref().map(|t| format!(" ({t})")).unwrap_or_default()
        ),
        RequestState::Sent { txid, .. } => format!("already sent ({txid})"),
        RequestState::BroadcastPending { txid, .. } => {
            format!("signed but not broadcast — retry with: sats tx broadcast {txid}")
        }
        RequestState::Failed { .. } => "failed before any signature".into(),
    }
}

/// One event onto the network's causal log.
pub(crate) fn journal(
    store: &Store,
    net_name: &str,
    request: &AgentRequest,
    kind: EventKind,
) -> Result<()> {
    store.append_event(
        net_name,
        &AgentEvent {
            format_version: EVENT_FORMAT_VERSION,
            at: unix_now(),
            network: net_name.to_string(),
            agent: request.agent.clone(),
            request_id: request.id.clone(),
            intent_digest: request.intent_digest.clone(),
            kind,
        },
    )
}

/// Post-write appends must not fail the operation: warn.
pub(crate) fn journal_soft(store: &Store, net_name: &str, request: &AgentRequest, kind: EventKind) {
    if let Err(err) = journal(store, net_name, request, kind) {
        eprintln!("⚠ event log append failed: {err:#}");
    }
}
