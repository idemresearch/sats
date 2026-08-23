//! The MCP tool surface. Denials are successful tool results with
//! `status: "denied"` — deterministic, machine-readable, and carrying the
//! exact message the agent should relay to its human.

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use sats_core::authz::{
    ApprovalDecision, DenyReason, Grant, IntentRequest, SpendRequest,
    authorize_intent_with_approval,
};
use sats_core::bitcoin::{Address, Network};
use sats_core::event::{AgentEvent, EVENT_FORMAT_VERSION, EventKind};
use sats_core::intent::SendIntent;
use sats_core::plan::{TransactionStatus, TxOrigin};
use sats_core::request::{AgentRequest, REQUEST_FORMAT_VERSION, RequestOutcome};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::commands::prepare;
use crate::config::{Config, network_name};
use crate::provider;
use crate::store::{Store, unix_now};
use crate::{keys, walletd};

#[derive(Clone)]
pub struct SatsMcp {
    dir: Option<PathBuf>,
    network: Network,
    agent: String,
    /// CLI --provider overrides the server was launched with; every tool
    /// call resolves providers the same way the CLI does.
    providers: Vec<provider::CliProvider>,
    tool_router: ToolRouter<Self>,
}

#[derive(Serialize, JsonSchema)]
pub struct BalanceResult {
    pub balance_sat: u64,
    pub pending_sat: u64,
    pub synced: bool,
    pub network: String,
}

#[derive(Serialize, JsonSchema)]
pub struct AddressResult {
    pub address: String,
    pub index: u32,
    pub network: String,
}

#[derive(Serialize, JsonSchema)]
pub struct GrantResult {
    /// False when the grant has been revoked or expired.
    pub active: bool,
    pub agent: String,
    pub network: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_sat: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent_sat: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_sat: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tx_sat: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_fee_sat: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SendParams {
    /// Recipient bitcoin address.
    pub address: String,
    /// Amount in satoshis.
    pub amount_sat: u64,
    /// Optional idempotency key: 1-64 characters of A-Za-z0-9_-. Retrying
    /// with the same key and the identical address and amount is safe — it
    /// returns the recorded outcome instead of paying twice. Reusing a key
    /// for a different send is a typed error.
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct SendResult {
    /// "sent", "denied", or "error".
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub txid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount_sat: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_sat: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_sat: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_budget_sat: Option<u64>,
    /// Denial code: expired, over_max_tx, over_max_fee, over_budget,
    /// revoked, approval_fee_exceeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Server-assigned request id: what a human reviews with
    /// `sats agent requests`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Typed operational error code: invalid_request_id,
    /// request_id_conflict, request_in_flight, request_incomplete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// True when this send was authorized by a one-time human approval
    /// rather than the grant's standing caps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via_approval: Option<bool>,
}

impl SendResult {
    fn sent(txid: String, amount_sat: u64, fee_sat: u64, remaining: u64) -> Self {
        SendResult {
            status: "sent".into(),
            txid: Some(txid),
            amount_sat: Some(amount_sat),
            fee_sat: Some(fee_sat),
            total_sat: Some(amount_sat + fee_sat),
            remaining_budget_sat: Some(remaining),
            reason: None,
            message: None,
            request_id: None,
            error_code: None,
            via_approval: None,
        }
    }

    fn denied(reason: &str, message: String) -> Self {
        SendResult {
            status: "denied".into(),
            txid: None,
            amount_sat: None,
            fee_sat: None,
            total_sat: None,
            remaining_budget_sat: None,
            reason: Some(reason.into()),
            message: Some(message),
            request_id: None,
            error_code: None,
            via_approval: None,
        }
    }

    fn error(message: String) -> Self {
        SendResult {
            status: "error".into(),
            txid: None,
            amount_sat: None,
            fee_sat: None,
            total_sat: None,
            remaining_budget_sat: None,
            reason: None,
            message: Some(message),
            request_id: None,
            error_code: None,
            via_approval: None,
        }
    }

    /// An operational error the agent can branch on mechanically.
    fn op_error(code: &str, message: String) -> Self {
        let mut result = SendResult::error(message);
        result.error_code = Some(code.into());
        result
    }

    fn with_request(mut self, id: &str) -> Self {
        self.request_id = Some(id.into());
        self
    }
}

/// The denial shape shared by every refusal path.
fn denial(reason: &DenyReason) -> SendResult {
    SendResult::denied(
        reason.code(),
        format!(
            "human authorization required: {}",
            reason.human().replace('\n', "; ")
        ),
    )
}

fn valid_request_key(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A denial the human can act on: cap denials name the exact one-time
/// exception command; expiry and revocation cannot be approved away.
fn denial_with_hint(reason: &DenyReason, request_id: &str) -> SendResult {
    let mut result = denial(reason);
    let approvable = matches!(
        reason,
        DenyReason::OverMaxTx { .. }
            | DenyReason::OverMaxFee { .. }
            | DenyReason::OverBudget { .. }
            | DenyReason::ApprovalFeeExceeded { .. }
    );
    if approvable && let Some(message) = &mut result.message {
        message.push_str(&format!(
            "; a human can approve exactly this request once with: sats agent approve {request_id}"
        ));
    }
    result.with_request(request_id)
}

/// The one approval that can authorize this intent right now, and a fresh
/// read of the record holding it. The executing request's own approval
/// wins; otherwise the lexicographically smallest holder, so concurrent
/// lookups pick the same one.
fn current_approval(
    store: &Store,
    net_name: &str,
    agent: &str,
    digest: &str,
    executing_id: &str,
    now: u64,
) -> Option<(sats_core::authz::IntentApproval, AgentRequest)> {
    if let Ok(Some(own)) = store.load_agent_request(net_name, agent, executing_id)
        && let Some(approval) = own.approval.clone()
        && approval.is_valid_for(digest, now)
    {
        return Some((approval, own));
    }
    let Ok(all) = store.list_agent_requests(net_name) else {
        return None;
    };
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

impl SatsMcp {
    pub fn new(
        dir: Option<PathBuf>,
        network: Network,
        agent: String,
        providers: Vec<provider::CliProvider>,
    ) -> Self {
        SatsMcp {
            dir,
            network,
            agent,
            providers,
            tool_router: Self::tool_router(),
        }
    }

    /// Run a blocking wallet operation off the async thread.
    async fn blocking<T, F>(&self, f: F) -> Result<T, ErrorData>
    where
        T: Send + 'static,
        F: FnOnce(Option<PathBuf>, Network, String, Vec<provider::CliProvider>) -> Result<T>
            + Send
            + 'static,
    {
        let (dir, network, agent, providers) = (
            self.dir.clone(),
            self.network,
            self.agent.clone(),
            self.providers.clone(),
        );
        tokio::task::spawn_blocking(move || f(dir, network, agent, providers))
            .await
            .map_err(|e| ErrorData::internal_error(format!("task failed: {e}"), None))?
            .map_err(|e| ErrorData::internal_error(format!("{e:#}"), None))
    }
}

#[tool_router]
impl SatsMcp {
    #[tool(
        description = "Get the wallet balance in satoshis. synced=false means the \
        chain could not be reached and the value is from cache."
    )]
    async fn get_balance(&self) -> Result<Json<BalanceResult>, ErrorData> {
        self.blocking(|dir, network, _agent, providers| {
            let store = Store::open(dir.as_deref())?;
            let config = Config::load(&store)?;
            let services = provider::resolve(&config, &providers, network)?;
            let mut ctx = walletd::open(&store, network)?;
            let synced = services.sync_wallet(&mut ctx).is_ok();
            let balance = ctx.wallet.balance();
            Ok(BalanceResult {
                balance_sat: (balance.confirmed + balance.trusted_pending).to_sat(),
                pending_sat: (balance.untrusted_pending + balance.immature).to_sat(),
                synced,
                network: ctx.net_name.to_string(),
            })
        })
        .await
        .map(Json)
    }

    #[tool(description = "Get a fresh receive address for this wallet.")]
    async fn get_receive_address(&self) -> Result<Json<AddressResult>, ErrorData> {
        self.blocking(|dir, network, _agent, _providers| {
            let store = Store::open(dir.as_deref())?;
            let mut ctx = walletd::open(&store, network)?;
            let info = ctx
                .wallet
                .reveal_next_address(bdk_wallet::KeychainKind::External);
            ctx.persist()?;
            Ok(AddressResult {
                address: info.address.to_string(),
                index: info.index,
                network: ctx.net_name.to_string(),
            })
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Get this agent's spending grant: budget, spent, remaining, \
        per-tx caps, and expiry. Check this before sending."
    )]
    async fn get_grant(&self) -> Result<Json<GrantResult>, ErrorData> {
        self.blocking(|dir, network, agent, _providers| {
            let store = Store::open(dir.as_deref())?;
            let net_name = network_name(network);
            let now = unix_now();
            let grant = store
                .load_grant(net_name, &agent)?
                .filter(|g| !g.is_expired(now));
            Ok(match grant {
                Some(g) => GrantResult {
                    active: true,
                    agent,
                    network: net_name.to_string(),
                    budget_sat: Some(g.budget_sat),
                    spent_sat: Some(g.spent_sat),
                    remaining_sat: Some(g.remaining_sat()),
                    max_tx_sat: g.max_tx_sat,
                    max_fee_sat: g.max_fee_sat,
                    tx_count: Some(g.tx_count),
                    expires_at: Some(g.expires_at),
                    message: None,
                },
                None => GrantResult {
                    active: false,
                    agent: agent.clone(),
                    network: net_name.to_string(),
                    budget_sat: None,
                    spent_sat: None,
                    remaining_sat: None,
                    max_tx_sat: None,
                    max_fee_sat: None,
                    tx_count: None,
                    expires_at: None,
                    message: Some(format!(
                        "no active grant — ask the human to run: sats agent grant {agent} --budget <sats>"
                    )),
                },
            })
        })
        .await
        .map(Json)
    }

    #[tool(description = "Send bitcoin. Enforced deterministically against the \
        human-authorized grant (budget, per-tx cap, fee cap, expiry). Returns \
        status='sent' with the txid, or status='denied' with the reason — a denial \
        means human authorization is required, not that you should retry unchanged. \
        Pass request_id (1-64 chars of A-Za-z0-9_-) to make retries safe: the same \
        key with the same address and amount never pays twice, and returns the \
        recorded outcome instead.")]
    async fn send(
        &self,
        Parameters(params): Parameters<SendParams>,
    ) -> Result<Json<SendResult>, ErrorData> {
        self.blocking(move |dir, network, agent, providers| {
            let store = Store::open(dir.as_deref())?;
            let config = Config::load(&store)?;
            Ok(execute_send(
                &store, &config, network, &agent, &providers, &params,
            ))
        })
        .await
        .map(Json)
    }
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

/// A "sent" result whose remaining budget reflects whatever grant state is
/// current — absent entirely when the grant is gone.
fn sent_result(txid: &str, amount_sat: u64, fee_sat: u64, grant: Option<&Grant>) -> SendResult {
    let mut result = SendResult::sent(txid.to_string(), amount_sat, fee_sat, 0);
    result.remaining_budget_sat = grant.map(Grant::remaining_sat);
    result
}

/// How the idempotency lookup resolved a send request.
enum ClaimState {
    /// Execute: a claimed record, and whether it was created just now.
    Execute(AgentRequest, crate::store::RequestClaim, bool),
    /// The recorded history already answers this request.
    Done(SendResult),
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
    params: &SendParams,
    recipient: &str,
    digest: &str,
    grant: Option<&Grant>,
    now: u64,
) -> ClaimState {
    let fresh_record = |id: String| AgentRequest {
        format_version: REQUEST_FORMAT_VERSION,
        id,
        network: net_name.to_string(),
        agent: agent.to_string(),
        client_request_id: params.request_id.clone(),
        recipient: recipient.to_string(),
        amount_sat: params.amount_sat,
        intent_digest: digest.to_string(),
        created_at: now,
        updated_at: now,
        outcome: None,
        approval: None,
        dismissed_at: None,
    };

    let Some(key) = &params.request_id else {
        // Keyless sends never replay; collisions on the random id retry.
        loop {
            let mut bytes = [0u8; 4];
            if let Err(err) = getrandom::fill(&mut bytes) {
                return ClaimState::Done(SendResult::error(format!(
                    "cannot generate request id: {err}"
                )));
            }
            let record = fresh_record(format!("r-{}", hex::encode(bytes)));
            match store.create_agent_request(net_name, &record) {
                Ok(Some(claim)) => return ClaimState::Execute(record, claim, true),
                Ok(None) => continue,
                Err(err) => return ClaimState::Done(SendResult::error(format!("{err:#}"))),
            }
        }
    };

    let id = format!("k-{key}");
    let existing = match store.load_agent_request(net_name, agent, &id) {
        Ok(existing) => existing,
        Err(err) => return ClaimState::Done(SendResult::error(format!("{err:#}"))),
    };
    let Some(mut existing) = existing else {
        let record = fresh_record(id);
        return match store.create_agent_request(net_name, &record) {
            Ok(Some(claim)) => ClaimState::Execute(record, claim, true),
            // Created between our load and now: a concurrent execution.
            Ok(None) => ClaimState::Done(
                SendResult::op_error(
                    "request_in_flight",
                    format!(
                        "request {} is executing right now — do not retry until it settles",
                        record.id
                    ),
                )
                .with_request(&record.id),
            ),
            Err(err) => ClaimState::Done(SendResult::error(format!("{err:#}"))),
        };
    };

    if existing.intent_digest != digest {
        journal_soft(store, net_name, &existing, EventKind::Conflicted);
        return ClaimState::Done(
            SendResult::op_error(
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
        let result = match &existing.outcome {
            Some(RequestOutcome::Sent { txid, fee_sat, .. }) => {
                sent_result(txid, existing.amount_sat, *fee_sat, grant)
            }
            Some(RequestOutcome::Failed { message, .. }) => SendResult::error(message.clone()),
            _ => SendResult::error("internal: side effect without outcome".into()),
        };
        return ClaimState::Done(result.with_request(&existing.id));
    }

    // No side effect on record: either mid-execution, crashed, or safe to
    // re-evaluate (a denial or a pre-signature failure).
    let claim = match store.claim_agent_request(net_name, agent, &id) {
        Ok(Some(claim)) => claim,
        Ok(None) => {
            return ClaimState::Done(
                SendResult::op_error(
                    "request_in_flight",
                    format!("request {id} is executing right now — do not retry until it settles"),
                )
                .with_request(&id),
            );
        }
        Err(err) => return ClaimState::Done(SendResult::error(format!("{err:#}"))),
    };
    if existing.outcome.is_none() {
        // Crashed mid-execution. A transaction attributed to this request
        // is the recorded truth; absent one, a human has to look.
        let records = match store.list_transactions(net_name) {
            Ok(records) => records,
            Err(err) => return ClaimState::Done(SendResult::error(format!("{err:#}"))),
        };
        let found = records.into_iter().find(|record| {
            record
                .origin
                .as_ref()
                .is_some_and(|origin| origin.request_id.as_deref() == Some(id.as_str()))
        });
        let result = match found {
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
                    sent_result(&record.txid, record.amount_sat, record.fee_sat, grant)
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
                    SendResult::error(message)
                }
            },
            None => SendResult::op_error(
                "request_incomplete",
                format!(
                    "request {id} started earlier but recorded no outcome and no transaction — a human should review: sats agent requests"
                ),
            ),
        };
        return ClaimState::Done(result.with_request(&id));
    }
    // A denial or pre-signature failure: side-effect free, so the retry
    // re-evaluates under the same id — this is what lets a human approval
    // land between the attempts.
    ClaimState::Execute(existing, claim, false)
}

/// The agent spend path: claim → prepare → authorize → reserve → sign →
/// persist → broadcast, journaled to the event log at every transition and
/// resolved onto the request record so a keyed retry can never pay twice.
/// Every failure mode maps to a deterministic result, never a panic.
fn execute_send(
    store: &Store,
    config: &Config,
    network: Network,
    agent: &str,
    providers: &[provider::CliProvider],
    params: &SendParams,
) -> SendResult {
    let net_name = network_name(network);
    let now = unix_now();

    if let Some(key) = &params.request_id
        && !valid_request_key(key)
    {
        return SendResult::op_error(
            "invalid_request_id",
            "request_id must be 1-64 characters of A-Za-z0-9_-".into(),
        );
    }

    // Normalize the recipient first: the canonical intent hashes one
    // spelling of the address, so textual variants deduplicate.
    let recipient = match Address::from_str(&params.address)
        .map_err(|e| format!("invalid address: {e}"))
        .and_then(|a| {
            a.require_network(network)
                .map_err(|_| format!("address is not valid for {net_name}"))
        }) {
        Ok(address) => address.to_string(),
        Err(message) => return SendResult::error(message),
    };
    let digest = SendIntent {
        network: net_name.to_string(),
        agent: agent.to_string(),
        recipient: recipient.clone(),
        amount_sat: params.amount_sat,
    }
    .digest();

    // Re-read the grant on every send so `sats agent revoke` takes effect
    // immediately, even mid-session. This copy is only for the precheck;
    // the authoritative read happens under the grant lock below.
    let grant = match store.load_grant(net_name, agent) {
        Ok(grant) => grant,
        Err(e) => return SendResult::error(format!("{e:#}")),
    };

    let (mut request, _claim, fresh) = match claim_request(
        store,
        net_name,
        agent,
        params,
        &recipient,
        &digest,
        grant.as_ref(),
        now,
    ) {
        ClaimState::Execute(request, claim, fresh) => (request, claim, fresh),
        ClaimState::Done(result) => return result,
    };

    // Revocation: the human already acted, and the signing key is gone
    // with the grant file. A fresh request is deliberately not recorded.
    let Some(grant) = grant else {
        let mut result = SendResult::denied(
            "revoked",
            format!(
                "human authorization required: no active grant — ask the human to run: sats agent grant {agent} --budget <sats>"
            ),
        );
        if !fresh {
            result = result.with_request(&request.id);
        }
        return result;
    };

    if fresh {
        // Nothing irreversible has happened yet: an unwritable audit log
        // fails the send closed.
        if let Err(err) = journal(
            store,
            net_name,
            &request,
            EventKind::RequestReceived {
                recipient: recipient.clone(),
                amount_sat: params.amount_sat,
            },
        ) {
            return SendResult::error(format!("cannot record request: {err:#}"));
        }
    }

    // Pre-check on the amount alone (fee 0): an obviously over-limit
    // request is denied deterministically, before any network access.
    let precheck = IntentRequest::Send(SpendRequest {
        amount_sat: params.amount_sat,
        fee_sat: 0,
    });
    let precheck_approval =
        current_approval(store, net_name, agent, &digest, &request.id, now).map(|(a, _)| a);
    if let ApprovalDecision::Deny(reason) =
        authorize_intent_with_approval(&grant, &precheck, &digest, precheck_approval.as_ref(), now)
    {
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
        return denial_with_hint(&reason, &request.id);
    }

    // Prepare the transaction to learn the real fee before any decision.
    let prepared_result = (|| -> Result<_> {
        let services = provider::resolve(config, providers, network)?;
        let mut ctx = walletd::open(store, network)?;
        // prepare::build syncs internally and hard-fails on stale state; the
        // agent request form carries no safety bypasses.
        let prepare_request =
            prepare::PrepareRequest::for_agent(&params.address, params.amount_sat);
        let prepared = prepare::build(&mut ctx, &services, &prepare_request)?;
        ctx.persist()?;
        Ok((ctx, services, prepared))
    })();
    let (mut ctx, services, prepared) = match prepared_result {
        Ok(v) => v,
        Err(e) => {
            let message = format!("{e:#}");
            record_outcome(
                store,
                net_name,
                &mut request,
                RequestOutcome::Failed {
                    message: message.clone(),
                    txid: None,
                    resolved_at: unix_now(),
                },
            );
            journal_soft(
                store,
                net_name,
                &request,
                EventKind::Failed {
                    message: message.clone(),
                },
            );
            return SendResult::error(message).with_request(&request.id);
        }
    };

    // The budget decision must be atomic with its persistence: hold the
    // grant lock across re-read → authorize → reserve → save (and any
    // refund) so concurrent sends cannot double-draw the budget.
    let grant_lock = match store.lock_grants(net_name) {
        Ok(lock) => lock,
        Err(e) => return SendResult::error(format!("cannot lock grant: {e:#}")),
    };
    let mut grant = match store.load_grant(net_name, agent) {
        Ok(Some(g)) => g,
        Ok(None) => {
            let message = "grant revoked before authorization".to_string();
            record_outcome_locked(
                store,
                net_name,
                &mut request,
                RequestOutcome::Failed {
                    message,
                    txid: None,
                    resolved_at: unix_now(),
                },
            );
            drop(grant_lock);
            return SendResult::denied(
                "revoked",
                "human authorization required: the grant was revoked".to_string(),
            )
            .with_request(&request.id);
        }
        Err(e) => return SendResult::error(format!("{e:#}")),
    };

    // Fresh clock and a fresh approval read under the lock: preparation
    // synced the chain and may have taken long enough for either to move.
    let now = unix_now();
    let holder = current_approval(store, net_name, agent, &digest, &request.id, now);
    let (mut approval, mut holder_record) = match holder {
        Some((approval, holder_record)) => (Some(approval), Some(holder_record)),
        None => (None, None),
    };
    let spend = SpendRequest {
        amount_sat: prepared.amount_sat,
        fee_sat: prepared.fee_sat,
    };

    // Reserve and persist the draw-down BEFORE signing: once a signature
    // exists the money must be considered spent.
    let via = match grant.reserve_with_approval(&spend, &digest, approval.as_mut(), now) {
        Ok(via) => via,
        Err(reason) => {
            record_outcome_locked(
                store,
                net_name,
                &mut request,
                RequestOutcome::Denied {
                    deny: reason.clone(),
                    resolved_at: now,
                },
            );
            drop(grant_lock);
            journal_soft(
                store,
                net_name,
                &request,
                EventKind::Denied {
                    deny: reason.clone(),
                    stage: "authorize".into(),
                },
            );
            return denial_with_hint(&reason, &request.id);
        }
    };
    if let sats_core::authz::ReserveVia::Approval = via {
        // Consume-before-reserve: the burned approval must hit disk on its
        // holder before the budget draw, so a crash burns the exception,
        // never doubles it.
        if let Some(consumed) = &mut approval {
            consumed.consumed_by_request = Some(request.id.clone());
        }
        let Some(mut holder_record) = holder_record.take() else {
            // Unreachable: an approval reservation implies a holder.
            return SendResult::error("internal: approval without a holder record".into())
                .with_request(&request.id);
        };
        holder_record.approval = approval.clone();
        holder_record.updated_at = now;
        if let Err(e) = store.save_agent_request(net_name, &holder_record) {
            return SendResult::error(format!("cannot consume approval: {e:#}"))
                .with_request(&request.id);
        }
        if holder_record.id == request.id {
            // Keep the in-memory executing record current so later outcome
            // writes cannot resurrect the unconsumed approval.
            request.approval = approval.clone();
        }
    }
    if let Err(e) = store.save_grant(net_name, &grant) {
        return SendResult::error(format!("cannot record spend: {e:#}")).with_request(&request.id);
    }
    if let Err(err) = journal(
        store,
        net_name,
        &request,
        EventKind::Reserved {
            total_sat: spend.total_sat(),
            remaining_sat: grant.remaining_sat(),
            via: match via {
                sats_core::authz::ReserveVia::Grant => "grant".into(),
                sats_core::authz::ReserveVia::Approval => "approval".into(),
            },
        },
    ) {
        // Nothing signed yet: refund and fail closed on a dead audit log.
        grant.refund(&spend);
        let _ = store.save_grant(net_name, &grant);
        return SendResult::error(format!("cannot record reservation: {err:#}"))
            .with_request(&request.id);
    }
    if let sats_core::authz::ReserveVia::Approval = via {
        journal_soft(
            store,
            net_name,
            &request,
            EventKind::ApprovalConsumed {
                consumed_by_request: request.id.clone(),
            },
        );
    }

    // Sign with the grant-wrapped seed.
    let signed = (|| -> Result<_> {
        let mnemonic = keys::unlock_grant(&grant, net_name)?;
        crate::spend::sign_psbt(&prepared, mnemonic, network)
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
                &mut request,
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
                &request,
                EventKind::Refunded {
                    total_sat: spend.total_sat(),
                },
            );
            journal_soft(
                store,
                net_name,
                &request,
                EventKind::Failed {
                    message: message.clone(),
                },
            );
            return SendResult::error(message).with_request(&request.id);
        }
    };
    // A signature exists: the reservation is final, no more grant writes.
    drop(grant_lock);

    let mut record = match prepared.into_transaction(psbt, None) {
        Ok(record) => record,
        Err(e) => {
            let message = format!("cannot finalize transaction: {e:#} — budget remains reserved");
            record_outcome(
                store,
                net_name,
                &mut request,
                RequestOutcome::Failed {
                    message: message.clone(),
                    txid: None,
                    resolved_at: unix_now(),
                },
            );
            journal_soft(
                store,
                net_name,
                &request,
                EventKind::Failed {
                    message: message.clone(),
                },
            );
            return SendResult::error(message).with_request(&request.id);
        }
    };
    record = record.with_origin(TxOrigin {
        surface: "mcp".into(),
        agent: Some(agent.to_string()),
        request_id: Some(request.id.clone()),
        intent_digest: Some(digest.clone()),
    });
    let txid = record.txid.clone();
    if let Err(e) = store.save_transaction(net_name, &record) {
        let message = format!("cannot save signed transaction: {e:#}");
        record_outcome(
            store,
            net_name,
            &mut request,
            RequestOutcome::Failed {
                message: message.clone(),
                txid: Some(txid.clone()),
                resolved_at: unix_now(),
            },
        );
        journal_soft(
            store,
            net_name,
            &request,
            EventKind::Failed {
                message: message.clone(),
            },
        );
        return SendResult::error(message).with_request(&request.id);
    }
    journal_soft(
        store,
        net_name,
        &request,
        EventKind::Signed { txid: txid.clone() },
    );

    match crate::spend::broadcast_record(store, &mut ctx, &services, &mut record) {
        Ok(broadcast_txid) => {
            journal_soft(
                store,
                net_name,
                &request,
                EventKind::Broadcast {
                    txid: broadcast_txid.to_string(),
                },
            );
            record_outcome(
                store,
                net_name,
                &mut request,
                RequestOutcome::Sent {
                    txid: broadcast_txid.to_string(),
                    fee_sat: spend.fee_sat,
                    resolved_at: unix_now(),
                },
            );
            let mut result = SendResult::sent(
                broadcast_txid.to_string(),
                spend.amount_sat,
                spend.fee_sat,
                grant.remaining_sat(),
            )
            .with_request(&request.id);
            if let sats_core::authz::ReserveVia::Approval = via {
                result.via_approval = Some(true);
            }
            result
        }
        // Signed but not broadcast: budget stays reserved (the signed tx
        // is out of our hands), and a human can retry the saved transaction.
        Err(e) => {
            let message = format!(
                "broadcast failed after signing: {e:#} — budget reserved; a human can retry with: sats tx broadcast {txid}"
            );
            journal_soft(
                store,
                net_name,
                &request,
                EventKind::BroadcastFailed {
                    txid: txid.clone(),
                    message: message.clone(),
                },
            );
            record_outcome(
                store,
                net_name,
                &mut request,
                RequestOutcome::Failed {
                    message: message.clone(),
                    txid: Some(txid),
                    resolved_at: unix_now(),
                },
            );
            SendResult::error(message).with_request(&request.id)
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SatsMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info = rmcp::model::Implementation::new("sats", env!("CARGO_PKG_VERSION"));
        info.with_instructions(format!(
            "sats: a Bitcoin wallet this server operates as agent {:?} on {}, under a \
             human-authorized spending grant. All amounts are integer satoshis. send() is \
             enforced deterministically against the grant's budget, per-transaction cap, fee \
             cap, and expiry; status='denied' means human authorization is required — relay \
             the message and request_id to your human instead of retrying unchanged. If the \
             human approves the request (sats agent approve), retry the identical send with \
             the same request_id: the one-time approval is consumed by exactly that intent. \
             Pass request_id on every send so retries can never pay twice. Use get_grant() \
             to see the remaining budget before sending.",
            self.agent,
            network_name(self.network),
        ))
    }
}
