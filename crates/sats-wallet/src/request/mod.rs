//! Agent requests as the native workflow object.
//!
//! Agents create requests. Humans authorize requests. sats executes
//! requests. This module is the reusable layer every surface calls:
//! [`create`] for the agent side (MCP today), [`dismiss`] and
//! [`execute`] for the human-authorized side (the CLI today; any trusted
//! approval surface tomorrow), and [`reconcile`] / [`reconcile_grant`]
//! for interrupted executions. Commands and tools are thin adapters
//! over it.
//!
//! Every read-then-write of grant or request state happens under the
//! grant lock, so grant issuance and revocation, request creation,
//! authorization, outcomes, and budget decisions serialize as one
//! history. A request is bound to the grant instance (`grant_id`) that
//! created it and never executes under another.
//!
//! Request ids are global: `r-` plus 32 hex characters (128 bits). A
//! filing derives its id from the grant, the agent, and the agent's
//! idempotency key, so the same key resolves to the same record within
//! one grant and to a fresh record under a re-issued grant. The key is
//! only the agent's retry handle — the id is what humans review,
//! approve, and dismiss.

pub mod execute;

use std::str::FromStr;

use anyhow::{Context, Result, bail};
use sats_core::authz::{
    AGENT_NAME_RULE, Decision, DenyReason, Grant, SpendRequest, evaluate_send, valid_agent_name,
};
use sats_core::bitcoin::hashes::{Hash, sha256};
use sats_core::bitcoin::{Address, Network};
use sats_core::event::{AgentEvent, EVENT_FORMAT_VERSION, EventKind};
use sats_core::intent::SendIntent;
use sats_core::plan::{TransactionRecord, TransactionStatus};
use sats_core::request::{AgentRequest, REQUEST_FORMAT_VERSION, RequestState};

use crate::config::network_name;
use crate::store::{Store, now_checked, unix_now};

/// The canonical rule for an agent-supplied idempotency key.
pub const REQUEST_KEY_RULE: &str = "idempotency_key must be 1-64 characters of A-Za-z0-9_-";

pub fn valid_request_key(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Whether `id` has the shape of a server request id: `r-` plus 32
/// lowercase hex characters.
pub fn is_request_id(id: &str) -> bool {
    id.len() == 34
        && id.starts_with("r-")
        && id.as_bytes()[2..]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

/// The server request id a filing resolves to: 128 bits of SHA-256
/// over the grant id, the agent, and the idempotency key. The grant id
/// is in the preimage so a key reused after a revoke and re-issue names
/// a new request — a new grant never inherits old requests — and two
/// agents with the same key never share an id.
pub fn keyed_request_id(grant_id: &str, agent: &str, key: &str) -> String {
    let mut preimage = Vec::with_capacity(64);
    preimage.extend_from_slice(b"sats-request-v1\0");
    preimage.extend_from_slice(grant_id.as_bytes());
    preimage.push(0);
    preimage.extend_from_slice(agent.as_bytes());
    preimage.push(0);
    preimage.extend_from_slice(key.as_bytes());
    let digest = sha256::Hash::hash(&preimage);
    format!("r-{}", hex::encode(&digest.as_byte_array()[..16]))
}

/// What an agent asks for. The token authenticates the agent against
/// its grant; nothing is written for a caller that cannot authenticate.
pub struct CreateParams<'a> {
    pub agent: &'a str,
    pub token: &'a str,
    /// The agent's retry handle: required, so a lost response or a
    /// retried tool call can never file twice.
    pub idempotency_key: &'a str,
    pub address: &'a str,
    pub amount_sat: u64,
}

/// Why a request could not be created. Operational conditions with a
/// stable code; a policy refusal is not an error — it is a `denied`
/// request the agent can observe.
#[derive(Debug)]
pub enum CreateError {
    InvalidAgent,
    InvalidIdempotencyKey,
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
            CreateError::InvalidIdempotencyKey => "invalid_idempotency_key",
            CreateError::InvalidAddress(_) => "invalid_address",
            CreateError::ClockUnavailable(_) => "clock_unavailable",
            CreateError::NoGrant => "no_grant",
            CreateError::Unauthorized => "unauthorized",
            CreateError::Conflict { .. } => "idempotency_key_conflict",
            CreateError::Store(_) => "store_error",
        }
    }

    pub fn message(&self, agent: &str) -> String {
        match self {
            CreateError::InvalidAgent => AGENT_NAME_RULE.to_string(),
            CreateError::InvalidIdempotencyKey => REQUEST_KEY_RULE.to_string(),
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
                "idempotency_key was already used for a different send ({request_id}) — pick a \
                 fresh key"
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
/// `grant_id`. The grant's reservation ledger is reconciled first, so
/// the verdict is never taken from accounting a crashed execution left
/// behind. The ladder then runs with the fee unknown (zero): a proposal
/// inside every boundary is recorded as `pending_approval`; a hard
/// boundary is recorded as `denied`. The audit line is appended
/// *before* the record is written, so no request can exist on disk
/// without its causal event. The same key with the same canonical
/// intent returns the existing record without writing anything; the
/// same key with a different intent is a conflict.
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
    if !valid_request_key(params.idempotency_key) {
        return Err(CreateError::InvalidIdempotencyKey);
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
    let mut grant = store
        .load_grant(net_name, params.agent)?
        .ok_or(CreateError::NoGrant)?;
    if !grant.authorizes(params.token) {
        return Err(CreateError::Unauthorized);
    }
    // A draw orphaned by a crashed execution must not deny this
    // request: settle the ledger before any policy decision.
    reconcile_grant_locked(store, net_name, &mut grant)?;

    let digest = SendIntent {
        network: net_name.to_string(),
        agent: params.agent.to_string(),
        recipient: recipient.clone(),
        amount_sat: params.amount_sat,
    }
    .digest();

    // A repeated key answers from the record: same grant, same intent,
    // same request. The id already commits to the grant, so a record
    // under another grant id at this path is a collision, answered as a
    // conflict rather than as someone else's request.
    let id = keyed_request_id(&grant.grant_id, params.agent, params.idempotency_key);
    if let Some(existing) = store.load_agent_request(net_name, params.agent, &id)? {
        if existing.grant_id != grant.grant_id || existing.intent_digest != digest {
            journal_soft(store, net_name, &existing, EventKind::Conflicted);
            return Err(CreateError::Conflict {
                request_id: existing.id,
            });
        }
        return Ok(existing);
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
    let record = AgentRequest {
        format_version: REQUEST_FORMAT_VERSION,
        id,
        network: net_name.to_string(),
        agent: params.agent.to_string(),
        grant_id: grant.grant_id.clone(),
        idempotency_key: Some(params.idempotency_key.to_string()),
        recipient: recipient.clone(),
        amount_sat: params.amount_sat,
        intent_digest: digest,
        created_at: now,
        updated_at: now,
        state,
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
            recipient,
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
/// the same posture as `sats agent revoke`. A request that provably
/// never reached the signer gives back any draw it still holds;
/// dismissing an `unresolved` request closes it without a refund, since
/// a signature may exist.
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
        // Refund before the state change: a crash between the two leaves
        // a pending request without a draw, which is consistent, rather
        // than a dismissed request whose draw can no longer be proven
        // unspent.
        if never_reached_signer(&fresh.state)
            && let Some(mut grant) = store.load_grant(net_name, &fresh.agent)?
        {
            release_orphan(store, net_name, &mut grant, &fresh)?;
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
        Some(grant) if grant.grant_id == request.grant_id => Ok(grant),
        _ => Err(DenyReason::Revoked),
    })
}

/// Whether the durable record proves the signer was never invoked for
/// the request's current attempt. `signing` is persisted before the
/// signer is invoked and every later state follows it, so a record in
/// any of these states has not crossed the boundary since its last
/// transition: a draw it still holds is an orphan of an interrupted
/// execution and can be returned.
pub(crate) fn never_reached_signer(state: &RequestState) -> bool {
    matches!(
        state,
        RequestState::PendingApproval | RequestState::Failed { .. } | RequestState::Denied { .. }
    )
}

/// Return the draw a request holds on its grant, exactly once, and
/// persist the grant. The caller holds the grant lock and has
/// established, from the durable record, that the request never reached
/// the signer. `Ok(false)` means the request held no draw.
pub(crate) fn release_orphan(
    store: &Store,
    net_name: &str,
    grant: &mut Grant,
    request: &AgentRequest,
) -> Result<bool> {
    let Some(spend) = grant.refund(&request.id) else {
        return Ok(false);
    };
    store.save_grant(net_name, grant)?;
    journal_soft(
        store,
        net_name,
        request,
        EventKind::Refunded {
            total_sat: spend.total_sat(),
        },
    );
    Ok(true)
}

/// Settle the reservation ledger of one agent's grant against its
/// request records.
///
/// A draw whose request provably never reached the signer — the record
/// is `pending_approval`, `failed`, or `denied` — was left by an
/// execution that died between persisting the reservation and
/// persisting `signing`, or between persisting `failed` and persisting
/// the refund. It is returned, once. Every other draw stays: `signing`,
/// `unresolved`, `sent`, `broadcast_pending`, `dismissed`, and a request
/// that cannot be read are all cases where a signature may exist.
/// Never refund on doubt.
pub fn reconcile_grant(store: &Store, net_name: &str, agent: &str) -> Result<()> {
    let _lock = store.lock_grants(net_name)?;
    let Some(mut grant) = store.load_grant(net_name, agent)? else {
        return Ok(());
    };
    reconcile_grant_locked(store, net_name, &mut grant)
}

/// [`reconcile_grant`] for a caller that already holds the grant lock
/// and the grant: the ledger is settled in place, and the grant is
/// persisted only if something was returned.
pub(crate) fn reconcile_grant_locked(
    store: &Store,
    net_name: &str,
    grant: &mut Grant,
) -> Result<()> {
    let agent = grant.agent.clone();
    let mut released = Vec::new();
    for entry in grant.reservations.clone() {
        let Ok(Some(request)) = store.load_agent_request(net_name, &agent, &entry.request_id)
        else {
            continue;
        };
        if never_reached_signer(&request.state)
            && let Some(spend) = grant.refund(&entry.request_id)
        {
            released.push((request, spend));
        }
    }
    if released.is_empty() {
        return Ok(());
    }
    store.save_grant(net_name, grant)?;
    for (request, spend) in released {
        journal_soft(
            store,
            net_name,
            &request,
            EventKind::Refunded {
                total_sat: spend.total_sat(),
            },
        );
    }
    Ok(())
}

/// The persisted transaction attributed to exactly this request: same
/// agent, same request id, same canonical intent. The id alone
/// identifies nothing.
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

/// Read-only execution visibility for a signing receipt. `Some(false)`
/// means a human must reconcile durable state; it never proves unsigned.
/// This snapshot does not change the request or contact a provider.
pub fn observe_execution(
    store: &Store,
    net_name: &str,
    request: &AgentRequest,
) -> Result<Option<bool>> {
    if matches!(request.state, RequestState::Signing { .. }) {
        store
            .request_execution_active(net_name, &request.agent, &request.id)
            .map(Some)
    } else {
        Ok(None)
    }
}

/// Settle an interrupted execution around the signature boundary.
///
/// A request left `signing` by a process that died is resolved from the
/// durable truth only. A persisted transaction attributed to it means a
/// signature exists: the request becomes `broadcast_pending` or `sent` by
/// the record's status. No transaction means the signer may or may not
/// have run: the request becomes `unresolved`, nothing is refunded, and
/// nothing is signed again — a human resolves it. A request whose
/// execution is still live (its claim is held) is left alone. A
/// `broadcast_pending` receipt is also repaired when the exact attributed
/// transaction already records successful broadcast.
pub fn reconcile(store: &Store, network: Network, request: AgentRequest) -> Result<AgentRequest> {
    let net_name = network_name(network);
    if !matches!(
        request.state,
        RequestState::Signing { .. } | RequestState::BroadcastPending { .. }
    ) {
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
    if !fresh.version_supported()
        || fresh.id != request.id
        || fresh.agent != request.agent
        || fresh.network != net_name
        || fresh.grant_id != request.grant_id
        || fresh.intent_digest != request.intent_digest
    {
        bail!("request changed during reconciliation; refusing to overwrite the record");
    }
    if let RequestState::BroadcastPending { txid, .. } = &fresh.state {
        // A successful broadcast can outlive its receipt write. Only the
        // exact attributed saved transaction can repair that bookkeeping.
        if let Some(record) = attributed_transaction(store, net_name, &fresh)?
            && record.txid == *txid
            && record.status == TransactionStatus::Broadcast
        {
            record_broadcast_receipt(store, net_name, &mut fresh, &record, now)?;
        }
        return Ok(fresh);
    }
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
            journal_soft(
                store,
                net_name,
                &fresh,
                EventKind::Unresolved {
                    message,
                    txid: None,
                },
            );
        }
    }
    Ok(fresh)
}

/// Every request for the network, interrupted executions settled first
/// and every active grant's reservation ledger reconciled.
pub fn list_reconciled(store: &Store, network: Network) -> Result<Vec<AgentRequest>> {
    let net_name = network_name(network);
    let requests = store
        .list_agent_requests(net_name)?
        .into_iter()
        .map(|request| reconcile(store, network, request))
        .collect::<Result<Vec<_>>>()?;
    for grant in store.active_grants(net_name, unix_now())? {
        reconcile_grant(store, net_name, &grant.agent)?;
    }
    Ok(requests)
}

/// Settle a `broadcast_pending` or abandoned `signing` request once its
/// transaction has been durably recorded as broadcast by another path (`sats tx broadcast`). The transaction's
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
    if record.status != TransactionStatus::Broadcast {
        return Ok(());
    }
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
    // Recovery must not race the process that still owns execution: it
    // could otherwise overwrite a repaired receipt with a stale outcome.
    let Some(_claim) = store.claim_agent_request(net_name, agent, request_id)? else {
        bail!("request {request_id} is still executing; wait before recovering its receipt");
    };
    let now = unix_now();
    let _lock = store.lock_grants(net_name)?;
    let Some(mut request) = store.load_agent_request(net_name, agent, request_id)? else {
        return Ok(());
    };
    if !request.version_supported()
        || request.id != request_id
        || request.agent != agent
        || request.network != net_name
        || request.intent_digest != digest
    {
        bail!("saved transaction attribution does not match the request record");
    }
    match &request.state {
        RequestState::BroadcastPending { txid, .. } if *txid == record.txid => {}
        RequestState::Signing { .. } => {}
        _ => return Ok(()),
    }
    record_broadcast_receipt(store, net_name, &mut request, &record, now)
}

/// The caller holds the grant lock and has verified exact attribution.
fn record_broadcast_receipt(
    store: &Store,
    net_name: &str,
    request: &mut AgentRequest,
    record: &TransactionRecord,
    now: u64,
) -> Result<()> {
    request.state = RequestState::Sent {
        txid: record.txid.clone(),
        fee_sat: record.fee_sat,
        at: now,
    };
    request.updated_at = now;
    store.save_agent_request(net_name, request)?;
    journal_soft(
        store,
        net_name,
        request,
        EventKind::Broadcast {
            txid: record.txid.clone(),
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
        RequestState::Signing { .. } => "signing or awaiting human reconciliation".into(),
        RequestState::Unresolved { txid, .. } => format!(
            "unresolved — a signature may exist{}; sats will not sign it again or refund it; \
             check sats status, then dismiss it",
            txid.as_ref().map(|t| format!(" ({t})")).unwrap_or_default()
        ),
        RequestState::Sent { txid, .. } => format!("already sent ({txid})"),
        RequestState::BroadcastPending { txid, .. } => {
            format!("signed; broadcast unconfirmed — recover with: sats tx broadcast {txid}")
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
        log::warn!("event log append failed: {err:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyed_ids_are_global_and_grant_scoped() {
        let a = keyed_request_id("g1", "alice", "invoice-1");
        assert!(is_request_id(&a), "{a}");
        assert_eq!(a, keyed_request_id("g1", "alice", "invoice-1"));
        assert_ne!(a, keyed_request_id("g1", "bob", "invoice-1"));
        assert_ne!(a, keyed_request_id("g2", "alice", "invoice-1"));
        assert_ne!(a, keyed_request_id("g1", "alice", "invoice-2"));
        // No separator ambiguity: shifting bytes across fields differs.
        assert_ne!(
            keyed_request_id("g1", "ab", "c"),
            keyed_request_id("g1", "a", "bc")
        );
    }

    #[test]
    fn request_id_shape() {
        assert!(is_request_id("r-0123456789abcdef0123456789abcdef"));
        assert!(!is_request_id("r-0123456789ABCDEF0123456789ABCDEF"));
        assert!(!is_request_id("r-0123456789abcdef0123456789abcde"));
        assert!(!is_request_id("r-0123456789abcdef"));
        assert!(!is_request_id("k-0123456789abcdef0123456789abcdef"));
        assert!(!is_request_id("invoice-1"));
        assert_eq!(keyed_request_id("g1", "alice", "k").len(), 34);
    }
}
