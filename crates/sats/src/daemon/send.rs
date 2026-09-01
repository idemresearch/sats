//! The agent spend path, as the daemon runs it.
//!
//! claim → precheck → *(caller prepares)* → derive → authorize → reserve
//! → sign → persist → *(caller broadcasts)* → close out. Every ordering
//! invariant of the pre-daemon path is preserved, and one is tighter:
//! the transaction record is now written by the same process that
//! produced the signature, before that signature is handed to anyone.
//!
//! Nothing a caller claims about a transaction is trusted. `BeginSend`
//! states a recipient and amount so a human-readable request record and
//! intent digest exist; [`authorize`] then re-derives both from the PSBT
//! and refuses if they disagree.

use anyhow::Result;
use sats_core::authz::{
    ApprovalDecision, DenyReason, Grant, IntentApproval, ReserveVia, SpendRequest,
    evaluate_send_with_approval,
};
use sats_core::bitcoin::Psbt;
use sats_core::event::{AgentEvent, EVENT_FORMAT_VERSION, EventKind};
use sats_core::intent::SendIntent;
use sats_core::plan::{TransactionRecord, TransactionStatus, TxOrigin};
use sats_core::request::{AgentRequest, REQUEST_FORMAT_VERSION, RequestOutcome};
use sats_core::signer::Signer;
use sats_core::verify;

use crate::daemon::protocol::{BroadcastOutcome, SendOutcome};
use crate::daemon::session::SigningKey;
use crate::store::{RequestClaim, Store, unix_now};

/// A claimed request, held for the life of one connection. Dropping it
/// releases the claim, so a caller that dies mid-send never strands the
/// request under a lock nobody holds.
pub struct InFlight {
    pub request: AgentRequest,
    pub digest: String,
    pub agent: String,
    pub recipient: String,
    pub amount_sat: u64,
    /// SHA-256 of the bearer token `BeginSend` authenticated. Later calls
    /// closing out this send must present the same token; the wire field
    /// on `Finish` is enforced against this, not decorative.
    token_hash: String,
    /// Set by [`authorize`] so [`finish`] can close out the right record.
    pub signed: Option<Signed>,
    _claim: RequestClaim,
}

impl InFlight {
    /// Whether a presented token is the one that began this send.
    /// Constant time in the token contents.
    pub fn authorizes(&self, token: &str) -> bool {
        sats_core::token::verify(token, &self.token_hash)
    }
}

/// What [`authorize`] produced, pending the caller's broadcast.
pub struct Signed {
    pub txid: String,
    pub spend: SpendRequest,
    pub via: ReserveVia,
    pub remaining_sat: u64,
}

pub enum Begin {
    /// Claimed and prechecked. Prepare, then call [`authorize`].
    Proceed(Box<InFlight>),
    /// The daemon resolved the request without preparing anything.
    Done(Box<SendOutcome>),
}

impl Begin {
    fn done(outcome: SendOutcome) -> Begin {
        Begin::Done(Box::new(outcome))
    }
}

pub fn valid_request_key(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A denial the human can act on: approvable refusals name the exact
/// one-time exception command; the hard envelope gets no hint, because
/// asking cannot move it. `DenyReason::approvable()` is the single
/// source of truth.
fn denial_with_hint(reason: &DenyReason, request_id: &str) -> SendOutcome {
    let mut outcome = SendOutcome::from_deny(reason);
    if reason.approvable()
        && let Some(message) = &mut outcome.message
    {
        message.push_str(&format!(
            "; a human can approve exactly this request once with: sats agent approve {request_id}"
        ));
    }
    outcome.with_request(request_id)
}

fn sent_outcome(txid: &str, amount_sat: u64, fee_sat: u64, grant: Option<&Grant>) -> SendOutcome {
    SendOutcome::sent(
        txid.to_string(),
        amount_sat,
        fee_sat,
        grant.map(Grant::remaining_sat),
    )
}

/// The recorded truth of a request that already produced a side effect.
fn replay_outcome(existing: &AgentRequest, grant: Option<&Grant>) -> SendOutcome {
    match &existing.outcome {
        Some(RequestOutcome::Sent { txid, fee_sat, .. }) => {
            sent_outcome(txid, existing.amount_sat, *fee_sat, grant)
        }
        Some(RequestOutcome::Failed { message, .. }) => SendOutcome::error(message.clone()),
        _ => SendOutcome::error("internal: side effect without outcome".into()),
    }
}

/// The denial for an agent with no grant on file.
fn revoked_outcome(agent: &str) -> SendOutcome {
    SendOutcome::denied(
        "revoked",
        format!(
            "human authorization required: no active grant — ask the human to run: sats agent grant {agent} --budget <sats>"
        ),
    )
}

/// One event onto the network's causal log.
fn journal(store: &Store, net_name: &str, request: &AgentRequest, kind: EventKind) -> Result<()> {
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

/// Post-signature and query-path appends must not fail the send: warn.
fn journal_soft(store: &Store, net_name: &str, request: &AgentRequest, kind: EventKind) {
    if let Err(err) = journal(store, net_name, request, kind) {
        eprintln!("⚠ event log append failed: {err:#}");
    }
}

/// Rewrite the request's outcome under a freshly taken grant lock.
fn record_outcome(
    store: &Store,
    net_name: &str,
    request: &mut AgentRequest,
    outcome: RequestOutcome,
) {
    match store.lock_grants(net_name) {
        Ok(_lock) => record_outcome_locked(store, net_name, request, outcome),
        Err(err) => eprintln!("⚠ request record not updated: {err:#}"),
    }
}

/// Rewrite the request's outcome while the caller holds the grant lock.
fn record_outcome_locked(
    store: &Store,
    net_name: &str,
    request: &mut AgentRequest,
    outcome: RequestOutcome,
) {
    request.outcome = Some(outcome);
    request.updated_at = unix_now();
    if let Err(err) = store.save_agent_request(net_name, request) {
        eprintln!("⚠ request record not updated: {err:#}");
    }
}

/// The candidate approval for this intent, and a fresh
/// read of the record holding it. The executing request's own approval
/// wins; otherwise the lexicographically smallest holder, so concurrent
/// lookups pick the same one. Callers must still check the current policy.
pub(crate) fn current_approval(
    store: &Store,
    net_name: &str,
    agent: &str,
    digest: &str,
    executing_id: &str,
    now: u64,
) -> Option<(IntentApproval, AgentRequest)> {
    if let Ok(Some(own)) = store.load_agent_request(net_name, agent, executing_id)
        && let Some(approval) = own.approval.clone()
        && approval.is_valid_for(digest, now)
    {
        return Some((approval, own));
    }
    let all = store.list_agent_requests(net_name).ok()?;
    let mut holders: Vec<AgentRequest> = all
        .into_iter()
        .filter(|record| record.agent == agent && record.id != executing_id)
        .filter(|record| {
            record
                .approval
                .as_ref()
                .is_some_and(|approval| approval.is_valid_for(digest, now))
        })
        .collect();
    holders.sort_by(|a, b| a.id.cmp(&b.id));
    holders
        .into_iter()
        .next()
        .and_then(|holder| holder.approval.clone().map(|approval| (approval, holder)))
}

/// How the idempotency lookup resolved a send request.
enum ClaimState {
    /// Execute: a claimed record, and whether it was created just now.
    Execute(AgentRequest, RequestClaim, bool),
    /// The recorded history already answers this request.
    Done(SendOutcome),
}

/// Resolve the request id — replaying, conflicting, or claiming execution.
/// The recorded truth outranks even revocation: a keyed retry whose earlier
/// execution signed a transaction learns that, never a denial that suggests
/// nothing happened.
#[allow(clippy::too_many_arguments)]
fn claim_request(
    store: &Store,
    net_name: &str,
    agent: &str,
    client_request_id: Option<&str>,
    recipient: &str,
    amount_sat: u64,
    digest: &str,
    grant: &Grant,
    now: u64,
) -> ClaimState {
    let fresh_record = |id: String| AgentRequest {
        format_version: REQUEST_FORMAT_VERSION,
        id,
        network: net_name.to_string(),
        agent: agent.to_string(),
        client_request_id: client_request_id.map(str::to_string),
        recipient: recipient.to_string(),
        amount_sat,
        intent_digest: digest.to_string(),
        created_at: now,
        updated_at: now,
        outcome: None,
        approval: None,
        dismissed_at: None,
    };

    let Some(key) = client_request_id else {
        // Keyless sends never replay; collisions on the random id retry.
        loop {
            let mut bytes = [0u8; 4];
            if let Err(err) = getrandom::fill(&mut bytes) {
                return ClaimState::Done(SendOutcome::error(format!(
                    "cannot generate request id: {err}"
                )));
            }
            let record = fresh_record(format!("r-{}", hex::encode(bytes)));
            match store.create_agent_request(net_name, &record) {
                Ok(Some(claim)) => return ClaimState::Execute(record, claim, true),
                Ok(None) => continue,
                Err(err) => return ClaimState::Done(SendOutcome::error(format!("{err:#}"))),
            }
        }
    };

    let id = format!("k-{key}");
    let existing = match store.load_agent_request(net_name, agent, &id) {
        Ok(existing) => existing,
        Err(err) => return ClaimState::Done(SendOutcome::error(format!("{err:#}"))),
    };
    let Some(mut existing) = existing else {
        let record = fresh_record(id);
        return match store.create_agent_request(net_name, &record) {
            Ok(Some(claim)) => ClaimState::Execute(record, claim, true),
            // Created between our load and now: a concurrent execution.
            Ok(None) => ClaimState::Done(
                SendOutcome::op_error(
                    "request_in_flight",
                    format!(
                        "request {} is executing right now — do not retry until it settles",
                        record.id
                    ),
                )
                .with_request(&record.id),
            ),
            Err(err) => ClaimState::Done(SendOutcome::error(format!("{err:#}"))),
        };
    };

    if existing.intent_digest != digest {
        journal_soft(store, net_name, &existing, EventKind::Conflicted);
        return ClaimState::Done(
            SendOutcome::op_error(
                "request_id_conflict",
                format!(
                    "request_id {key:?} was already used for a different send — pick a fresh id"
                ),
            )
            .with_request(&existing.id),
        );
    }
    if existing.has_side_effect() {
        journal_soft(store, net_name, &existing, EventKind::Replayed);
        return ClaimState::Done(replay_outcome(&existing, Some(grant)).with_request(&existing.id));
    }

    // No side effect on record: either mid-execution, crashed, or safe to
    // re-evaluate (a denial or a pre-signature failure).
    let claim = match store.claim_agent_request(net_name, agent, &id) {
        Ok(Some(claim)) => claim,
        Ok(None) => {
            return ClaimState::Done(
                SendOutcome::op_error(
                    "request_in_flight",
                    format!("request {id} is executing right now — do not retry until it settles"),
                )
                .with_request(&id),
            );
        }
        Err(err) => return ClaimState::Done(SendOutcome::error(format!("{err:#}"))),
    };
    if existing.outcome.is_none() {
        // Crashed mid-execution. A transaction attributed to this request
        // is the recorded truth; absent one, a human has to look.
        let records = match store.list_transactions(net_name) {
            Ok(records) => records,
            Err(err) => return ClaimState::Done(SendOutcome::error(format!("{err:#}"))),
        };
        let found = records.into_iter().find(|record| {
            record
                .origin
                .as_ref()
                .is_some_and(|origin| origin.request_id.as_deref() == Some(id.as_str()))
        });
        let outcome = match found {
            Some(record) => match record.status {
                TransactionStatus::Broadcast => {
                    record_outcome(
                        store,
                        net_name,
                        &mut existing,
                        RequestOutcome::Sent {
                            txid: record.txid.clone(),
                            fee_sat: record.fee_sat,
                            resolved_at: now,
                        },
                    );
                    sent_outcome(&record.txid, record.amount_sat, record.fee_sat, Some(grant))
                }
                TransactionStatus::Pending => {
                    let message = format!(
                        "signed but broadcast unconfirmed — a human can retry with: sats tx broadcast {}",
                        record.txid
                    );
                    record_outcome(
                        store,
                        net_name,
                        &mut existing,
                        RequestOutcome::Failed {
                            message: message.clone(),
                            txid: Some(record.txid.clone()),
                            resolved_at: now,
                        },
                    );
                    SendOutcome::error(message)
                }
            },
            None => SendOutcome::op_error(
                "request_incomplete",
                format!(
                    "request {id} started earlier but recorded no outcome and no transaction — a human should review: sats agent requests"
                ),
            ),
        };
        return ClaimState::Done(outcome.with_request(&id));
    }
    // A denial or pre-signature failure: side-effect free, so the retry
    // re-evaluates under the same id — this is what lets a human approval
    // land between the attempts.
    ClaimState::Execute(existing, claim, false)
}

/// Claim the request and run the amount-only precheck.
///
/// The precheck exists so an obviously impossible request is refused
/// before the caller spends a chain sync on it. It is never the decision:
/// [`authorize`] re-runs the full check against the derived fee.
pub fn begin(
    store: &Store,
    net_name: &str,
    agent: &str,
    token: &str,
    client_request_id: Option<&str>,
    recipient: &str,
    amount_sat: u64,
) -> Begin {
    // The agent name becomes a path component in every store lookup, so
    // it is gated before anything touches the disk. A typed operational
    // error, not a denial: a malformed name is protocol misuse, never
    // something a human approves.
    if !sats_core::authz::valid_agent_name(agent) {
        return Begin::done(SendOutcome::op_error(
            "invalid_agent",
            sats_core::authz::AGENT_NAME_RULE.into(),
        ));
    }

    if let Some(key) = client_request_id
        && !valid_request_key(key)
    {
        return Begin::done(SendOutcome::op_error(
            "invalid_request_id",
            "request_id must be 1-64 characters of A-Za-z0-9_-".into(),
        ));
    }

    // Authorization time fails closed: a broken clock must not un-expire
    // every grant by reporting 1970.
    let now = match crate::store::now_checked() {
        Ok(now) => now,
        Err(e) => return Begin::done(SendOutcome::op_error("clock_unavailable", format!("{e:#}"))),
    };

    let digest = SendIntent {
        network: net_name.to_string(),
        agent: agent.to_string(),
        recipient: recipient.to_string(),
        amount_sat,
    }
    .digest();

    // Re-read the grant on every send so `sats agent revoke` takes effect
    // immediately, even mid-session. The read also rewrites a durable
    // record still spelling the removed autonomous mode as `ask`, so any
    // use of a grant retires the legacy shape from disk. This copy is
    // only for the precheck; the authoritative read happens under the
    // grant lock in `authorize`.
    let grant = match store.normalize_grant_autonomy(net_name, agent) {
        Ok(grant) => grant,
        Err(e) => return Begin::done(SendOutcome::error(format!("{e:#}"))),
    };
    // A grant that exists but rejects this token means the caller is not
    // the agent it claims to be. Refuse before writing anything: an
    // unauthenticated caller must not be able to create request records.
    if grant.as_ref().is_some_and(|g| !g.authorizes(token)) {
        return Begin::done(SendOutcome::op_error(
            "unauthorized",
            format!(
                "the presented token does not authorize agent {agent:?} — a grant issued or \
                 replaced later has a different token"
            ),
        ));
    }
    // No grant on file: nobody can be authenticated, so nothing may be
    // written — no request record, no journal line. Recorded truth still
    // outranks revocation: a keyed retry whose earlier execution signed
    // learns that from the existing record, read-only.
    let Some(grant) = grant else {
        if let Some(key) = client_request_id {
            let id = format!("k-{key}");
            match store.load_agent_request(net_name, agent, &id) {
                Ok(Some(existing)) if existing.intent_digest != digest => {
                    return Begin::done(
                        SendOutcome::op_error(
                            "request_id_conflict",
                            format!(
                                "request_id {key:?} was already used for a different send — pick a fresh id"
                            ),
                        )
                        .with_request(&existing.id),
                    );
                }
                Ok(Some(existing)) if existing.has_side_effect() => {
                    return Begin::done(replay_outcome(&existing, None).with_request(&existing.id));
                }
                Ok(Some(existing)) => {
                    return Begin::done(revoked_outcome(agent).with_request(&existing.id));
                }
                Ok(None) => {}
                Err(e) => return Begin::done(SendOutcome::error(format!("{e:#}"))),
            }
        }
        return Begin::done(revoked_outcome(agent));
    };

    let (mut request, claim, fresh) = match claim_request(
        store,
        net_name,
        agent,
        client_request_id,
        recipient,
        amount_sat,
        &digest,
        &grant,
        now,
    ) {
        ClaimState::Execute(request, claim, fresh) => (request, claim, fresh),
        ClaimState::Done(outcome) => return Begin::done(outcome),
    };

    if fresh {
        // Nothing irreversible has happened yet: an unwritable audit log
        // fails the send closed.
        if let Err(err) = journal(
            store,
            net_name,
            &request,
            EventKind::RequestReceived {
                recipient: recipient.to_string(),
                amount_sat,
            },
        ) {
            return Begin::done(SendOutcome::error(format!(
                "cannot record request: {err:#}"
            )));
        }
    }

    // Pre-check on the amount alone (fee 0): an obviously over-limit
    // request is denied deterministically, before any network access.
    // The full ladder runs — recipient rule included — so a refusal any
    // later stage would repeat is surfaced before a chain sync.
    let precheck = SpendRequest {
        amount_sat,
        fee_sat: 0,
    };
    let precheck_approval =
        current_approval(store, net_name, agent, &digest, &request.id, now).map(|(a, _)| a);
    if let ApprovalDecision::Deny(reason) = evaluate_send_with_approval(
        &grant,
        recipient,
        &precheck,
        &digest,
        precheck_approval.as_ref(),
        now,
    ) {
        record_outcome(
            store,
            net_name,
            &mut request,
            RequestOutcome::Denied {
                deny: reason.clone(),
                resolved_at: unix_now(),
            },
        );
        journal_soft(
            store,
            net_name,
            &request,
            EventKind::Denied {
                deny: reason.clone(),
                stage: "precheck".into(),
            },
        );
        return Begin::done(denial_with_hint(&reason, &request.id));
    }

    Begin::Proceed(Box::new(InFlight {
        request,
        digest,
        agent: agent.to_string(),
        recipient: recipient.to_string(),
        amount_sat,
        token_hash: grant.token_hash.clone(),
        signed: None,
        _claim: claim,
    }))
}

/// Derive, authorize, reserve, sign, and persist.
///
/// The PSBT is the only source of truth for what is being spent. The
/// amount and fee are recomputed from it; the recipient and amount stated
/// at `BeginSend` — which the request record and intent digest already
/// committed to — must match, or nothing is signed.
///
/// `make_signer` is the signer boundary: it is invoked exactly once, and
/// only after the reservation has authorized and persisted the spend, so
/// no denied or unapproved request can ever construct a signer. The
/// daemon passes the in-memory `LocalSigner`; tests pass a counting
/// probe to prove the boundary holds.
// The parameters mirror the protocol stages one-to-one, and the signer
// factory is a deliberate extra: bundling them would only hide the seam.
#[allow(clippy::too_many_arguments)]
pub fn authorize(
    store: &Store,
    key: &SigningKey,
    net_name: &str,
    token: &str,
    flight: &mut InFlight,
    psbt: &str,
    excluded_utxos: u64,
    make_signer: impl FnOnce() -> Result<Box<dyn Signer>>,
) -> SendOutcome {
    let request_id = flight.request.id.clone();
    let fail = |store: &Store, flight: &mut InFlight, message: String| -> SendOutcome {
        record_outcome(
            store,
            net_name,
            &mut flight.request,
            RequestOutcome::Failed {
                message: message.clone(),
                txid: None,
                resolved_at: unix_now(),
            },
        );
        journal_soft(
            store,
            net_name,
            &flight.request,
            EventKind::Failed {
                message: message.clone(),
            },
        );
        SendOutcome::error(message).with_request(&request_id)
    };

    let psbt: Psbt = match psbt.parse() {
        Ok(psbt) => psbt,
        Err(e) => return fail(store, flight, format!("invalid psbt: {e}")),
    };

    // What this transaction actually does, per our own descriptors.
    let derived = match verify::derive_intent(&psbt, &key.external, &key.internal, key.network) {
        Ok(derived) => derived,
        Err(e) => return fail(store, flight, format!("refusing to sign: {e}")),
    };
    let (recipient, amount_sat) = match derived.sole_recipient() {
        Ok(pair) => pair,
        Err(e) => return fail(store, flight, format!("refusing to sign: {e}")),
    };
    // The digest and the human-visible request record committed to one
    // exact payment. A PSBT that pays anything else is not this request.
    if recipient != flight.recipient || amount_sat != flight.amount_sat {
        return fail(
            store,
            flight,
            format!(
                "refusing to sign: the prepared transaction pays {} sat to {recipient}, \
                 but this request is {} sat to {}",
                amount_sat, flight.amount_sat, flight.recipient
            ),
        );
    }

    // The budget decision must be atomic with its persistence: hold the
    // grant lock across re-read → authorize → reserve → save (and any
    // refund) so concurrent sends cannot double-draw the budget.
    let grant_lock = match store.lock_grants(net_name) {
        Ok(lock) => lock,
        Err(e) => return SendOutcome::error(format!("cannot lock grant: {e:#}")),
    };
    // Re-read under the lock: the grant may have been revoked or replaced
    // since `BeginSend`, and a replacement carries a different token.
    let mut grant = match store.load_grant(net_name, &flight.agent) {
        Ok(Some(g)) if g.authorizes(token) => g,
        Ok(_) => {
            let message = "grant revoked before authorization".to_string();
            record_outcome_locked(
                store,
                net_name,
                &mut flight.request,
                RequestOutcome::Failed {
                    message,
                    txid: None,
                    resolved_at: unix_now(),
                },
            );
            drop(grant_lock);
            return SendOutcome::denied(
                "revoked",
                "human authorization required: the grant was revoked".to_string(),
            )
            .with_request(&request_id);
        }
        Err(e) => return SendOutcome::error(format!("{e:#}")),
    };

    // Fresh clock and a fresh approval read under the lock: preparation
    // synced the chain and may have taken long enough for either to move.
    // The clock fails closed, like at `begin`.
    let now = match crate::store::now_checked() {
        Ok(now) => now,
        Err(e) => {
            drop(grant_lock);
            return SendOutcome::op_error("clock_unavailable", format!("{e:#}"))
                .with_request(&request_id);
        }
    };
    let (mut approval, mut holder_record) = match current_approval(
        store,
        net_name,
        &flight.agent,
        &flight.digest,
        &request_id,
        now,
    ) {
        Some((approval, holder)) => (Some(approval), Some(holder)),
        None => (None, None),
    };
    let spend = SpendRequest {
        amount_sat: derived.sats_out,
        fee_sat: derived.fee_sat,
    };

    // Reserve and persist the draw-down BEFORE signing: once a signature
    // exists the money must be considered spent. The reserve re-runs the
    // full ladder, recipient included.
    let via = match grant.reserve_send(
        &flight.recipient,
        &spend,
        &flight.digest,
        approval.as_mut(),
        now,
    ) {
        Ok(via) => via,
        Err(reason) => {
            record_outcome_locked(
                store,
                net_name,
                &mut flight.request,
                RequestOutcome::Denied {
                    deny: reason.clone(),
                    resolved_at: now,
                },
            );
            drop(grant_lock);
            journal_soft(
                store,
                net_name,
                &flight.request,
                EventKind::Denied {
                    deny: reason.clone(),
                    stage: "authorize".into(),
                },
            );
            return denial_with_hint(&reason, &request_id);
        }
    };
    if let ReserveVia::Approval = via {
        // Consume-before-reserve: the burned approval must hit disk on its
        // holder before the budget draw, so a crash burns the exception,
        // never doubles it.
        if let Some(consumed) = &mut approval {
            consumed.consumed_by_request = Some(request_id.clone());
        }
        let Some(mut holder) = holder_record.take() else {
            // Unreachable: an approval reservation implies a holder.
            return SendOutcome::error("internal: approval without a holder record".into())
                .with_request(&request_id);
        };
        holder.approval = approval.clone();
        holder.updated_at = now;
        if let Err(e) = store.save_agent_request(net_name, &holder) {
            return SendOutcome::error(format!("cannot consume approval: {e:#}"))
                .with_request(&request_id);
        }
        if holder.id == request_id {
            // Keep the in-memory executing record current so later outcome
            // writes cannot resurrect the unconsumed approval.
            flight.request.approval = approval.clone();
        }
    }
    if let Err(e) = store.save_grant(net_name, &grant) {
        return SendOutcome::error(format!("cannot record spend: {e:#}")).with_request(&request_id);
    }
    if let Err(err) = journal(
        store,
        net_name,
        &flight.request,
        EventKind::Reserved {
            total_sat: spend.total_sat(),
            remaining_sat: grant.remaining_sat(),
            via: match via {
                ReserveVia::Grant => "grant".into(),
                ReserveVia::Approval => "approval".into(),
            },
        },
    ) {
        // Nothing signed yet: refund and fail closed on a dead audit log.
        grant.refund(&spend);
        let _ = store.save_grant(net_name, &grant);
        return SendOutcome::error(format!("cannot record reservation: {err:#}"))
            .with_request(&request_id);
    }
    if let ReserveVia::Approval = via {
        journal_soft(
            store,
            net_name,
            &flight.request,
            EventKind::ApprovalConsumed {
                consumed_by_request: request_id.clone(),
            },
        );
    }

    // Sign. The signer — and any key material inside it — is constructed
    // only here, after the reservation, and exists only for this block.
    let signed = (|| -> Result<Psbt> {
        let mut psbt = psbt;
        let mut signer = make_signer()?;
        if !signer.sign(&mut psbt)? {
            anyhow::bail!("signer produced an unfinalized transaction");
        }
        Ok(psbt)
    })();
    let psbt = match signed {
        Ok(psbt) => psbt,
        Err(e) => {
            // No signature exists — safe to refund the reservation. The
            // consumed approval stays consumed: budget comes back, the
            // exception does not.
            grant.refund(&spend);
            let _ = store.save_grant(net_name, &grant);
            let message = format!("signing failed: {e:#}");
            record_outcome_locked(
                store,
                net_name,
                &mut flight.request,
                RequestOutcome::Failed {
                    message: message.clone(),
                    txid: None,
                    resolved_at: unix_now(),
                },
            );
            drop(grant_lock);
            journal_soft(
                store,
                net_name,
                &flight.request,
                EventKind::Refunded {
                    total_sat: spend.total_sat(),
                },
            );
            journal_soft(
                store,
                net_name,
                &flight.request,
                EventKind::Failed {
                    message: message.clone(),
                },
            );
            return SendOutcome::error(message).with_request(&request_id);
        }
    };
    // A signature exists: the reservation is final, no more grant writes.
    let remaining_sat = grant.remaining_sat();
    drop(grant_lock);

    // The txid excludes witness data, so it is fixed before extraction. A
    // signature exists from here on: any failure must record the txid, so
    // a keyed retry replays instead of signing a second transaction.
    let signed_txid = psbt.unsigned_tx.compute_txid().to_string();
    let tx = match psbt.extract_tx() {
        Ok(tx) => tx,
        Err(e) => {
            let message = format!("cannot finalize transaction: {e} — budget remains reserved");
            record_outcome(
                store,
                net_name,
                &mut flight.request,
                RequestOutcome::Failed {
                    message: message.clone(),
                    txid: Some(signed_txid),
                    resolved_at: unix_now(),
                },
            );
            journal_soft(
                store,
                net_name,
                &flight.request,
                EventKind::Failed {
                    message: message.clone(),
                },
            );
            return SendOutcome::error(message).with_request(&request_id);
        }
    };
    // Amount and fee come from the derivation, not from the caller.
    let record = TransactionRecord::from_transaction(
        net_name.to_string(),
        recipient.to_string(),
        spend.amount_sat,
        spend.fee_sat,
        unix_now(),
        excluded_utxos,
        None,
        &tx,
    )
    .with_origin(TxOrigin {
        surface: "mcp".into(),
        agent: Some(flight.agent.clone()),
        request_id: Some(request_id.clone()),
        intent_digest: Some(flight.digest.clone()),
    });
    let txid = record.txid.clone();

    // Persist before the signature leaves this process, so a caller that
    // dies holding the only copy cannot strand it.
    if let Err(e) = store.save_transaction(net_name, &record) {
        let message = format!("cannot save signed transaction: {e:#}");
        record_outcome(
            store,
            net_name,
            &mut flight.request,
            RequestOutcome::Failed {
                message: message.clone(),
                txid: Some(txid.clone()),
                resolved_at: unix_now(),
            },
        );
        journal_soft(
            store,
            net_name,
            &flight.request,
            EventKind::Failed {
                message: message.clone(),
            },
        );
        return SendOutcome::error(message).with_request(&request_id);
    }
    journal_soft(
        store,
        net_name,
        &flight.request,
        EventKind::Signed { txid: txid.clone() },
    );

    flight.signed = Some(Signed {
        txid: txid.clone(),
        spend,
        via,
        remaining_sat,
    });

    // The caller broadcasts by reading the record just written: a signed
    // transaction never crosses the socket.
    let mut outcome = SendOutcome::sent(txid, spend.amount_sat, spend.fee_sat, Some(remaining_sat))
        .with_request(&request_id);
    if let ReserveVia::Approval = via {
        outcome.via_approval = Some(true);
    }
    outcome
}

/// Close out a signed send once the caller has tried to broadcast it.
///
/// A broadcast failure is never a refund: the transaction is signed and
/// spendable regardless of whether the provider accepted it.
pub fn finish(
    store: &Store,
    net_name: &str,
    flight: &mut InFlight,
    broadcast: BroadcastOutcome,
) -> SendOutcome {
    let request_id = flight.request.id.clone();
    let Some(signed) = &flight.signed else {
        // Nothing was signed: the caller failed while preparing. Record
        // it as a side-effect-free failure so a keyed retry — or a human
        // approval landing in between — can still re-evaluate this id.
        let BroadcastOutcome::Failed { message } = broadcast else {
            return SendOutcome::op_error(
                "not_signed",
                "a broadcast was reported for a request that was never signed".into(),
            )
            .with_request(&request_id);
        };
        record_outcome(
            store,
            net_name,
            &mut flight.request,
            RequestOutcome::Failed {
                message: message.clone(),
                txid: None,
                resolved_at: unix_now(),
            },
        );
        journal_soft(
            store,
            net_name,
            &flight.request,
            EventKind::Failed {
                message: message.clone(),
            },
        );
        return SendOutcome::error(message).with_request(&request_id);
    };
    let (txid, spend, via, remaining) = (
        signed.txid.clone(),
        signed.spend,
        signed.via,
        signed.remaining_sat,
    );

    match broadcast {
        BroadcastOutcome::Broadcast {
            txid: broadcast_txid,
        } => {
            if let Ok(mut record) = store.load_transaction(net_name, &txid) {
                record.mark_broadcast();
                if let Err(err) = store.save_transaction(net_name, &record) {
                    eprintln!(
                        "⚠ broadcast succeeded but the local record was not updated: {err:#}"
                    );
                }
            }
            journal_soft(
                store,
                net_name,
                &flight.request,
                EventKind::Broadcast {
                    txid: broadcast_txid.clone(),
                },
            );
            record_outcome(
                store,
                net_name,
                &mut flight.request,
                RequestOutcome::Sent {
                    txid: broadcast_txid.clone(),
                    fee_sat: spend.fee_sat,
                    resolved_at: unix_now(),
                },
            );
            let mut outcome = SendOutcome::sent(
                broadcast_txid,
                spend.amount_sat,
                spend.fee_sat,
                Some(remaining),
            )
            .with_request(&request_id);
            if let ReserveVia::Approval = via {
                outcome.via_approval = Some(true);
            }
            outcome
        }
        // Signed but not broadcast: budget stays reserved (the signed tx
        // is out of our hands), and a human can retry the saved transaction.
        BroadcastOutcome::Failed { message } => {
            let message = format!(
                "broadcast failed after signing: {message} — budget reserved; \
                 a human can retry with: sats tx broadcast {txid}"
            );
            journal_soft(
                store,
                net_name,
                &flight.request,
                EventKind::BroadcastFailed {
                    txid: txid.clone(),
                    message: message.clone(),
                },
            );
            record_outcome(
                store,
                net_name,
                &mut flight.request,
                RequestOutcome::Failed {
                    message: message.clone(),
                    txid: Some(txid),
                    resolved_at: unix_now(),
                },
            );
            SendOutcome::error(message).with_request(&request_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::str::FromStr;
    use std::time::Duration;

    use bdk_wallet::Wallet;
    use bdk_wallet::bitcoin::hashes::Hash;
    use bdk_wallet::bitcoin::{Address, Amount, BlockHash, Network};
    use bdk_wallet::chain::{BlockId, ConfirmationBlockTime};
    use bdk_wallet::test_utils::{insert_checkpoint, receive_output};
    use sats_core::authz::{GRANT_FORMAT_VERSION, GrantMode};
    use sats_core::error::SignerError;
    use sats_core::signer::LocalSigner;
    use sats_core::{seal, seed, token};

    use super::*;
    use crate::daemon::session::Session;
    use crate::store::AAD_SEED;

    const MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const ADDRESS: &str = "tb1pvlnw9n2zuefmxzwmuz0763uajw8nmaattkhd8002g3ekejjspxtshu2q9n";

    /// A signing key obtained the way the daemon obtains one: seal the
    /// seed into the store, unlock a session, take the handle.
    fn unlocked_key(store: &Store) -> SigningKey {
        let blob = seal::seal(MNEMONIC.as_bytes(), b"pw", AAD_SEED).unwrap();
        store.write_seed(&blob).unwrap();
        let session = Session::new(Network::Signet, "signet", Duration::from_secs(600));
        session.unlock(store, "pw").unwrap();
        session.signing_key().unwrap()
    }

    /// Persist a capless signet grant for "claude" and hand back the one
    /// emission of its bearer token.
    fn grant_with_token(store: &Store, budget_sat: u64) -> String {
        let minted = token::generate().unwrap();
        let now = unix_now();
        let grant = Grant {
            format_version: GRANT_FORMAT_VERSION,
            agent: "claude".into(),
            network: "signet".into(),
            budget_sat,
            spent_sat: 0,
            max_tx_sat: None,
            max_fee_sat: None,
            created_at: now,
            expires_at: now + 3_600,
            tx_count: 0,
            token_id: minted.token_id.clone(),
            token_hash: minted.token_hash.clone(),
            mode: Default::default(),
            ask_max_tx_sat: None,
            allowed_recipients: None,
            suspended: None,
            strikes: Vec::new(),
        };
        store.save_grant("signet", &grant).unwrap();
        minted.secret.to_string()
    }

    /// Mutate the stored grant in place, the way control-plane commands do.
    fn update_grant(store: &Store, mutate: impl FnOnce(&mut Grant)) {
        let mut grant = store.load_grant("signet", "claude").unwrap().unwrap();
        mutate(&mut grant);
        store.save_grant("signet", &grant).unwrap();
    }

    /// The canonical digest `begin` computes for a send to [`ADDRESS`].
    fn digest_for(amount_sat: u64) -> String {
        SendIntent {
            network: "signet".into(),
            agent: "claude".into(),
            recipient: ADDRESS.into(),
            amount_sat,
        }
        .digest()
    }

    /// Arm a one-time approval on a recorded request, the way
    /// `sats agent approve` does: digest-bound, unconsumed, under an
    /// explicit fee ceiling.
    fn approve_request(store: &Store, id: &str, amount_sat: u64, max_fee_sat: u64) {
        let mut record = store
            .load_agent_request("signet", "claude", id)
            .unwrap()
            .unwrap();
        let now = unix_now();
        record.approval = Some(IntentApproval {
            intent_digest: digest_for(amount_sat),
            approved_at: now,
            expires_at: now + 600,
            max_fee_sat,
            consumed_at: None,
            consumed_by_request: None,
        });
        record.dismissed_at = None;
        record.updated_at = now;
        store.save_agent_request("signet", &record).unwrap();
    }

    /// The signer-boundary probe: counts `sign` invocations before
    /// delegating to the real in-memory signer.
    struct CountingSigner {
        signs: Rc<Cell<usize>>,
        inner: LocalSigner,
    }

    impl Signer for CountingSigner {
        fn name(&self) -> &'static str {
            "counting"
        }

        fn sign(&mut self, psbt: &mut Psbt) -> Result<bool, SignerError> {
            self.signs.set(self.signs.get() + 1);
            self.inner.sign(psbt)
        }
    }

    fn counting_factory(
        signs: Rc<Cell<usize>>,
    ) -> impl FnOnce() -> anyhow::Result<Box<dyn Signer>> {
        move || {
            let mnemonic = seed::parse_mnemonic(MNEMONIC).unwrap();
            Ok(Box::new(CountingSigner {
                signs,
                inner: LocalSigner::new(mnemonic, Network::Signet),
            }))
        }
    }

    /// Run one send attempt end to end through `begin` + `authorize`,
    /// counting every signer invocation it causes.
    fn attempt(
        store: &Store,
        key: &SigningKey,
        token: &str,
        request_key: &str,
        amount_sat: u64,
        fee_sat: u64,
        signs: &Rc<Cell<usize>>,
    ) -> SendOutcome {
        match begin(
            store,
            "signet",
            "claude",
            token,
            Some(request_key),
            ADDRESS,
            amount_sat,
        ) {
            Begin::Proceed(mut flight) => authorize(
                store,
                key,
                "signet",
                token,
                &mut flight,
                &signable_psbt(amount_sat, fee_sat).to_string(),
                0,
                counting_factory(signs.clone()),
            ),
            Begin::Done(outcome) => *outcome,
        }
    }

    /// A wallet-owned, signable PSBT paying [`ADDRESS`] with an exact
    /// absolute fee. An absurd fee (over `Psbt::extract_tx`'s 25k sat/vB
    /// ceiling) is the one way to reach the post-signature extraction
    /// failure; a sane fee exercises the ordinary signing path.
    fn signable_psbt(amount_sat: u64, fee_sat: u64) -> Psbt {
        let mnemonic = seed::parse_mnemonic(MNEMONIC).unwrap();
        let (external, internal) = seed::public_descriptors(&mnemonic, Network::Signet).unwrap();
        let mut wallet = Wallet::create(external, internal)
            .network(Network::Signet)
            .create_wallet_no_persist()
            .unwrap();
        let block_900 = BlockId {
            height: 900,
            hash: BlockHash::all_zeros(),
        };
        insert_checkpoint(&mut wallet, block_900);
        insert_checkpoint(
            &mut wallet,
            BlockId {
                height: 1_000,
                hash: BlockHash::all_zeros(),
            },
        );
        receive_output(
            &mut wallet,
            Amount::from_sat(20_000_000),
            ConfirmationBlockTime {
                block_id: block_900,
                confirmation_time: 100,
            },
        );
        let recipient = Address::from_str(ADDRESS)
            .unwrap()
            .require_network(Network::Signet)
            .unwrap();
        let mut builder = wallet.build_tx();
        builder.add_recipient(recipient.script_pubkey(), Amount::from_sat(amount_sat));
        builder.fee_absolute(Amount::from_sat(fee_sat));
        builder.finish().unwrap()
    }

    fn event_kinds(store: &Store) -> Vec<String> {
        store
            .list_event_lines("signet")
            .unwrap()
            .into_iter()
            .map(|line| match line {
                crate::store::EventLine::Event(event) => event.kind_str().to_string(),
                crate::store::EventLine::Unknown(_) => "unknown".to_string(),
            })
            .collect()
    }

    /// The full extraction-failure path, live: signing succeeds, extract
    /// refuses the absurd fee, the recorded outcome carries the txid the
    /// signature fixed — so a keyed retry replays instead of producing a
    /// second signature, and the reservation stands.
    #[test]
    fn extract_failure_records_the_txid_and_the_keyed_retry_replays() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        let key = unlocked_key(&store);
        let token = grant_with_token(&store, 20_000_000);
        let amount_sat = 1_000;
        let fee_sat = 9_000_000; // ~58k sat/vB on this tx, over the 25k ceiling
        let psbt = signable_psbt(amount_sat, fee_sat);
        let expected_txid = psbt.unsigned_tx.compute_txid().to_string();

        // Every send asks first: the novel attempt records the request
        // and denies, a human approval arms it, the identical retry runs.
        match begin(
            &store,
            "signet",
            "claude",
            &token,
            Some("stuck"),
            ADDRESS,
            amount_sat,
        ) {
            Begin::Done(outcome) => assert_eq!(outcome.reason.as_deref(), Some("ask_required")),
            Begin::Proceed(_) => panic!("an unapproved send must not proceed"),
        }
        approve_request(&store, "k-stuck", amount_sat, fee_sat);
        let mut flight = match begin(
            &store,
            "signet",
            "claude",
            &token,
            Some("stuck"),
            ADDRESS,
            amount_sat,
        ) {
            Begin::Proceed(flight) => flight,
            Begin::Done(outcome) => panic!("begin refused: {outcome:?}"),
        };
        assert!(flight.authorizes(&token));
        assert!(!flight.authorizes(&"1".repeat(64)), "wrong token");
        assert!(!flight.authorizes("not-hex"), "malformed token");

        let signs = Rc::new(Cell::new(0));
        let outcome = authorize(
            &store,
            &key,
            "signet",
            &token,
            &mut flight,
            &psbt.to_string(),
            0,
            counting_factory(signs.clone()),
        );
        assert_eq!(signs.get(), 1, "the approved send signed exactly once");
        assert_eq!(outcome.status, "error");
        assert!(
            outcome
                .message
                .as_deref()
                .unwrap()
                .contains("budget remains reserved"),
            "got: {:?}",
            outcome.message
        );
        assert!(flight.signed.is_none(), "extraction failed before handoff");

        // The recorded truth names the transaction the signature created.
        let record = store
            .load_agent_request("signet", "claude", "k-stuck")
            .unwrap()
            .unwrap();
        assert!(record.has_side_effect(), "a signature is a side effect");
        match &record.outcome {
            Some(RequestOutcome::Failed {
                txid: Some(txid), ..
            }) => assert_eq!(txid, &expected_txid),
            other => panic!("expected a signed failure, got {other:?}"),
        }
        // The reservation stands: the signed transaction is out of reach,
        // not undone.
        let grant = store.load_grant("signet", "claude").unwrap().unwrap();
        assert_eq!(grant.spent_sat, amount_sat + fee_sat);

        // A keyed retry replays the recorded outcome; nothing re-executes.
        drop(flight);
        let replay = match begin(
            &store,
            "signet",
            "claude",
            &token,
            Some("stuck"),
            ADDRESS,
            amount_sat,
        ) {
            Begin::Done(outcome) => *outcome,
            Begin::Proceed(_) => panic!("a signed failure must replay, never re-execute"),
        };
        assert_eq!(replay.status, "error");
        assert_eq!(replay.request_id.as_deref(), Some("k-stuck"));
        assert!(
            replay
                .message
                .as_deref()
                .unwrap()
                .contains("budget remains reserved")
        );
        let grant = store.load_grant("signet", "claude").unwrap().unwrap();
        assert_eq!(grant.spent_sat, amount_sat + fee_sat, "no second draw");
        assert_eq!(
            event_kinds(&store),
            [
                "request_received",
                "denied",
                "reserved",
                "approval_consumed",
                "failed",
                "replayed"
            ]
        );
    }

    /// The signer boundary, probed: every deny class ends with zero
    /// signer invocations, even with an approval in hand where one could
    /// plausibly be presented.
    #[test]
    fn no_unapproved_send_reaches_the_signer() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        let key = unlocked_key(&store);
        let token = grant_with_token(&store, 20_000_000);
        let signs = Rc::new(Cell::new(0));

        // Distinct amounts per row: an approval binds the intent digest
        // (recipient + amount), so identical intents would share one.

        // Ask grant, no approval: the terminal ask.
        let denied = attempt(&store, &key, &token, "plain", 1_000, 300, &signs);
        assert_eq!(denied.reason.as_deref(), Some("ask_required"));
        assert_eq!(denied.approvable, Some(true));

        // Observe grant: read-only, an armed approval lifts nothing.
        update_grant(&store, |g| g.mode = GrantMode::Observe);
        let denied = attempt(&store, &key, &token, "observe", 1_100, 300, &signs);
        assert_eq!(denied.reason.as_deref(), Some("observe_only"));
        approve_request(&store, "k-observe", 1_100, 600);
        let denied = attempt(&store, &key, &token, "observe", 1_100, 300, &signs);
        assert_eq!(denied.reason.as_deref(), Some("observe_only"));
        update_grant(&store, |g| g.mode = GrantMode::Ask);

        // A migrated legacy-auto record: what used to execute unattended
        // now asks, and the durable file is rewritten as ask by the send.
        let path = store.grants_dir("signet").join("claude.json");
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        raw["mode"] = serde_json::json!("auto");
        std::fs::write(&path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
        let denied = attempt(&store, &key, &token, "legacy", 1_200, 300, &signs);
        assert_eq!(denied.reason.as_deref(), Some("ask_required"));
        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(!rewritten.contains("auto"), "legacy autonomy retired");

        // Recipient outside the allowlist.
        update_grant(&store, |g| {
            g.allowed_recipients = Some(vec!["tb1pother".into()])
        });
        let denied = attempt(&store, &key, &token, "stranger", 1_300, 300, &signs);
        assert_eq!(denied.reason.as_deref(), Some("recipient_not_allowed"));
        update_grant(&store, |g| g.allowed_recipients = None);

        // Above the hard ceiling: an armed approval lifts nothing.
        update_grant(&store, |g| g.ask_max_tx_sat = Some(500));
        let denied = attempt(&store, &key, &token, "ceiling", 1_400, 300, &signs);
        assert_eq!(denied.reason.as_deref(), Some("over_ask_max"));
        assert_eq!(denied.approvable, Some(false));
        approve_request(&store, "k-ceiling", 1_400, 600);
        let denied = attempt(&store, &key, &token, "ceiling", 1_400, 300, &signs);
        assert_eq!(denied.reason.as_deref(), Some("over_ask_max"));
        update_grant(&store, |g| g.ask_max_tx_sat = None);

        // Expired grant.
        update_grant(&store, |g| g.expires_at = 1);
        let denied = attempt(&store, &key, &token, "expired", 1_500, 300, &signs);
        assert_eq!(denied.reason.as_deref(), Some("expired"));

        assert_eq!(signs.get(), 0, "no denied request may reach the signer");
        // And none of it drew budget or produced a transaction.
        update_grant(&store, |g| g.expires_at = unix_now() + 3_600);
        let grant = store.load_grant("signet", "claude").unwrap().unwrap();
        assert_eq!(grant.spent_sat, 0);
        assert!(store.list_transactions("signet").unwrap().is_empty());
    }

    /// Mutating an approved intent — a different amount under the same
    /// key, or a PSBT paying something else than the approved request —
    /// never reaches the signer, and revocation between begin and
    /// authorize wins.
    #[test]
    fn mutations_and_revocation_never_reach_the_signer() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        let key = unlocked_key(&store);
        let token = grant_with_token(&store, 20_000_000);
        let signs = Rc::new(Cell::new(0));

        // Record and approve exactly 1_000 sat.
        let denied = attempt(&store, &key, &token, "mut", 1_000, 300, &signs);
        assert_eq!(denied.reason.as_deref(), Some("ask_required"));
        approve_request(&store, "k-mut", 1_000, 600);

        // Same key, different amount: the digest refuses before anything.
        let conflict = attempt(&store, &key, &token, "mut", 2_000, 300, &signs);
        assert_eq!(conflict.error_code.as_deref(), Some("request_id_conflict"));

        // Same request, but the prepared PSBT pays a different amount:
        // the begin-time commitment refuses before the reservation.
        let mut flight = match begin(
            &store,
            "signet",
            "claude",
            &token,
            Some("mut"),
            ADDRESS,
            1_000,
        ) {
            Begin::Proceed(flight) => flight,
            Begin::Done(outcome) => panic!("approved begin refused: {outcome:?}"),
        };
        let wrong = signable_psbt(2_000, 300);
        let outcome = authorize(
            &store,
            &key,
            "signet",
            &token,
            &mut flight,
            &wrong.to_string(),
            0,
            counting_factory(signs.clone()),
        );
        assert_eq!(outcome.status, "error");
        assert!(
            outcome
                .message
                .as_deref()
                .unwrap()
                .contains("refusing to sign"),
            "got: {:?}",
            outcome.message
        );
        drop(flight);

        // Revoked between begin and authorize: the under-lock re-read wins.
        let mut flight = match begin(
            &store,
            "signet",
            "claude",
            &token,
            Some("mut"),
            ADDRESS,
            1_000,
        ) {
            Begin::Proceed(flight) => flight,
            Begin::Done(outcome) => panic!("approved begin refused: {outcome:?}"),
        };
        store.delete_grant("signet", "claude").unwrap();
        let outcome = authorize(
            &store,
            &key,
            "signet",
            &token,
            &mut flight,
            &signable_psbt(1_000, 300).to_string(),
            0,
            counting_factory(signs.clone()),
        );
        assert_eq!(outcome.reason.as_deref(), Some("revoked"));

        assert_eq!(signs.get(), 0, "no mutation may reach the signer");
        assert!(store.list_transactions("signet").unwrap().is_empty());
    }

    /// The one legitimate path: a valid digest-bound approval plus the
    /// identical retry signs exactly once, and neither the keyed replay
    /// nor a fresh identical send signs again.
    #[test]
    fn an_approved_identical_retry_signs_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        let key = unlocked_key(&store);
        let token = grant_with_token(&store, 20_000_000);
        let signs = Rc::new(Cell::new(0));

        let denied = attempt(&store, &key, &token, "pay", 1_000, 300, &signs);
        assert_eq!(denied.reason.as_deref(), Some("ask_required"));
        assert_eq!(signs.get(), 0);
        approve_request(&store, "k-pay", 1_000, 600);

        // The identical retry signs, once, via the approval.
        let mut flight = match begin(
            &store,
            "signet",
            "claude",
            &token,
            Some("pay"),
            ADDRESS,
            1_000,
        ) {
            Begin::Proceed(flight) => flight,
            Begin::Done(outcome) => panic!("approved begin refused: {outcome:?}"),
        };
        let outcome = authorize(
            &store,
            &key,
            "signet",
            &token,
            &mut flight,
            &signable_psbt(1_000, 300).to_string(),
            0,
            counting_factory(signs.clone()),
        );
        assert_eq!(outcome.status, "sent", "got: {outcome:?}");
        assert_eq!(outcome.via_approval, Some(true));
        assert_eq!(signs.get(), 1);
        let txid = outcome.txid.clone().unwrap();
        let closed = finish(
            &store,
            "signet",
            &mut flight,
            BroadcastOutcome::Broadcast { txid: txid.clone() },
        );
        assert_eq!(closed.status, "sent");
        drop(flight);

        // The keyed replay returns the recorded truth without signing.
        let replay = attempt(&store, &key, &token, "pay", 1_000, 300, &signs);
        assert_eq!(replay.status, "sent");
        assert_eq!(replay.txid.as_deref(), Some(txid.as_str()));
        assert_eq!(signs.get(), 1, "a replay never signs");

        // A fresh identical send finds the approval consumed and asks.
        let fresh = attempt(&store, &key, &token, "pay-2", 1_000, 300, &signs);
        assert_eq!(fresh.reason.as_deref(), Some("ask_required"));
        assert_eq!(signs.get(), 1, "a consumed approval never signs again");

        let grant = store.load_grant("signet", "claude").unwrap().unwrap();
        assert_eq!(grant.spent_sat, 1_300, "exactly one draw");
        assert_eq!(store.list_transactions("signet").unwrap().len(), 1);
    }
}
