//! The MCP tool surface: what an agent can do with the wallet.
//!
//! Agents create requests. Humans authorize requests. sats executes
//! requests. This process reads the wallet and files requests under the
//! grant its bearer token names; it holds no key material, never
//! prepares or signs a transaction, and never broadcasts. A request the
//! grant refuses is a successful tool result with `status: "denied"` —
//! deterministic, machine-readable, and terminal. A request inside the
//! grant is `status: "pending_approval"`: filed for a human, not failed.

use std::path::PathBuf;

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use sats_core::bitcoin::Network;
use sats_core::request::{AgentRequest, RequestState};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::config::{Config, network_name};
use crate::provider;
use crate::request::{self, CreateParams};
use crate::store::{Store, unix_now};
use crate::walletd;

/// Environment variable carrying the agent's bearer token, as printed
/// once by `sats agent grant`.
pub const TOKEN_ENV: &str = "SATS_AGENT_TOKEN";

#[derive(Deserialize, JsonSchema)]
pub struct CheckRequestParams {
    /// The request_id a request_send result returned: r- plus 32 hex
    /// characters. Not the idempotency_key you passed in.
    pub request_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct RequestSendParams {
    /// Recipient bitcoin address.
    pub address: String,
    /// Amount in satoshis.
    pub amount_sat: u64,
    /// Your idempotency key for this send: 1-64 characters of
    /// A-Za-z0-9_-, required. Calling again with the same key and the
    /// identical address and amount returns the existing request instead
    /// of filing a second one, so a lost response or a retried call never
    /// files twice. Reusing a key for a different send is a typed error.
    /// This is not the request_id: the result carries that.
    pub idempotency_key: String,
}

/// The state of one request, as the agent sees it. The same shape is
/// returned by `request_send` and `check_request`.
#[derive(Serialize, JsonSchema)]
pub struct RequestView {
    /// The request's lifecycle state: pending_approval (filed, awaiting
    /// the human), denied (outside the grant; terminal), dismissed (the
    /// human declined; terminal), signing (the human authorized it and
    /// sats is signing), sent (broadcast; carries txid), broadcast_pending
    /// (signed, awaiting a rebroadcast by the human; carries txid),
    /// unresolved (a signature may exist; the human resolves it),
    /// failed (execution stopped before any signature; the human may
    /// authorize again). Also "error" for a typed operational condition
    /// on this call, and "not_found" for an unknown request id.
    pub status: String,
    /// Server-assigned request id: what a human reviews with
    /// `sats agent requests`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Denial code, when denied: expired, over_max_tx, over_max_fee,
    /// over_budget, amount_overflow, observe_only, recipient_not_allowed,
    /// revoked (the grant that created the request is gone). Every one is
    /// a grant boundary no approval lifts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Typed operational error code, on status "error": invalid_agent,
    /// invalid_idempotency_key, invalid_address, clock_unavailable,
    /// no_grant, unauthorized, idempotency_key_conflict, store_error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount_sat: Option<u64>,
    /// The fee the executed transaction paid, once one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_sat: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub txid: Option<String>,
    pub message: String,
}

impl RequestView {
    fn error(code: &str, message: String) -> Self {
        RequestView {
            status: "error".into(),
            request_id: None,
            reason: None,
            error_code: Some(code.into()),
            recipient: None,
            amount_sat: None,
            fee_sat: None,
            txid: None,
            message,
        }
    }

    fn not_found(message: String) -> Self {
        RequestView {
            status: "not_found".into(),
            request_id: None,
            reason: None,
            error_code: None,
            recipient: None,
            amount_sat: None,
            fee_sat: None,
            txid: None,
            message,
        }
    }

    /// The agent-facing view of a durable record.
    pub fn of(record: &AgentRequest) -> Self {
        let (reason, fee_sat, txid, message) = match &record.state {
            RequestState::PendingApproval => (
                None,
                None,
                None,
                format!(
                    "filed for human review — the human approves with: sats agent approve {}; \
                     poll check_request to observe the result, and do not file it again",
                    record.id
                ),
            ),
            RequestState::Denied { deny, .. } => (
                Some(deny.code().to_string()),
                None,
                None,
                format!(
                    "outside the grant: {} — no approval lifts a grant boundary; only the \
                     human changing the grant can",
                    deny.human().replace('\n', "; ")
                ),
            ),
            RequestState::Dismissed { .. } => (
                None,
                None,
                None,
                "the human dismissed this request — do not file it again; ask your human \
                 before proposing it again"
                    .into(),
            ),
            RequestState::Signing { .. } => (
                None,
                None,
                None,
                "the human authorized it; sats is signing and broadcasting — poll \
                 check_request"
                    .into(),
            ),
            RequestState::Unresolved { txid, message, .. } => (
                None,
                None,
                txid.clone(),
                format!(
                    "unresolved: {message} — the human resolves it; nothing for you to do, \
                     and do not file it again"
                ),
            ),
            RequestState::Sent { txid, fee_sat, .. } => (
                None,
                Some(*fee_sat),
                Some(txid.clone()),
                "sent — this request is complete".into(),
            ),
            RequestState::BroadcastPending { txid, fee_sat, .. } => (
                None,
                Some(*fee_sat),
                Some(txid.clone()),
                "signed but not yet broadcast — the human retries the broadcast; nothing \
                 for you to do"
                    .into(),
            ),
            RequestState::Failed { message, .. } => (
                None,
                None,
                None,
                format!(
                    "execution stopped before any signature: {message} — the human may \
                     authorize it again"
                ),
            ),
        };
        RequestView {
            status: record.status().into(),
            request_id: Some(record.id.clone()),
            reason,
            error_code: None,
            recipient: Some(record.recipient.clone()),
            amount_sat: Some(record.amount_sat),
            fee_sat,
            txid,
            message,
        }
    }
}

#[derive(Clone)]
pub struct SatsMcp {
    dir: Option<PathBuf>,
    network: Network,
    agent: String,
    /// CLI --provider overrides the server was launched with; chain-reading
    /// tools resolve them when called. Local tools never resolve providers.
    providers: Vec<provider::CliProvider>,
    /// The grant's bearer token. It names a policy; it opens nothing and
    /// cannot recover the seed.
    token: Zeroizing<String>,
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
    /// Authority mode: "ask" (every request waits for a human) or
    /// "observe" (read-only). There is no autonomous mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Standing recipient allowlist. Absent means any recipient may be
    /// proposed; listed recipients are the only ones the grant accepts —
    /// others are the hard denial recipient_not_allowed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_recipients: Option<Vec<String>>,
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

impl SatsMcp {
    pub fn new(
        dir: Option<PathBuf>,
        network: Network,
        agent: String,
        providers: Vec<provider::CliProvider>,
        token: Zeroizing<String>,
    ) -> Self {
        SatsMcp {
            dir,
            network,
            agent,
            providers,
            token,
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
        chain could not be reached and the value is from cache.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
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

    // Not read-only: revealing an address advances and persists the
    // wallet's derivation index.
    #[tool(
        description = "Get a fresh receive address for this wallet.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
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
        description = "Get this agent's grant: budget, spent, remaining, per-tx caps, \
        mode, recipient allowlist, and expiry. Check this before filing a request.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
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
                    mode: Some(g.mode.as_str().to_string()),
                    allowed_recipients: g.allowed_recipients.clone(),
                    budget_sat: Some(g.budget_sat),
                    spent_sat: Some(g.spent_sat),
                    remaining_sat: Some(g.remaining_sat()),
                    max_tx_sat: g.max_tx_sat,
                    max_fee_sat: Some(g.max_fee_sat),
                    tx_count: Some(g.tx_count),
                    expires_at: Some(g.expires_at),
                    message: None,
                },
                None => GrantResult {
                    active: false,
                    agent: agent.clone(),
                    network: net_name.to_string(),
                    mode: None,
                    allowed_recipients: None,
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

    #[tool(
        description = "File a request to send bitcoin. You cannot cause a signature: \
        a request is executed only when a human authorizes it, in their own process, \
        and you take no further action after filing — the normal result is \
        status='pending_approval' with a request_id. Relay the request_id to your \
        human and observe the outcome with check_request. Every grant boundary is \
        hard and enforced deterministically (budget, per-tx amount cap, fee cap, \
        expiry, mode, recipient allowlist): status='denied' names a boundary no \
        approval lifts — only the human changing the grant can, so do not file it \
        again. idempotency_key (1-64 chars of A-Za-z0-9_-) is required and is your \
        retry handle: a repeated call with the same key, address, and amount returns \
        the existing request instead of filing a second one. The result's request_id \
        is the server's id — use it with check_request; never pass it back as a key.",
        // Client-side hints only: the grant's ladder and the human's
        // authorization are the security boundary. A client that ignores
        // them changes nothing about what can be signed.
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = false)
    )]
    async fn request_send(
        &self,
        Parameters(params): Parameters<RequestSendParams>,
    ) -> Result<Json<RequestView>, ErrorData> {
        let token = self.token.clone();
        self.blocking(move |dir, network, agent, _providers| {
            let store = Store::open(dir.as_deref())?;
            let created = request::create(
                &store,
                network,
                &CreateParams {
                    agent: &agent,
                    token: &token,
                    idempotency_key: &params.idempotency_key,
                    address: &params.address,
                    amount_sat: params.amount_sat,
                },
            );
            Ok(match created {
                Ok(record) => RequestView::of(&record),
                Err(err) => {
                    let mut view = RequestView::error(err.code(), err.message(&agent));
                    if let request::CreateError::Conflict { request_id } = &err {
                        view.request_id = Some(request_id.clone());
                    }
                    view
                }
            })
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Check the state of one of your own requests, by the request_id \
        a request_send result returned (not your idempotency_key). This is the \
        sanctioned way to observe a human decision: it reads the durable record only — \
        no chain access, no side effects. pending_approval means the human has not \
        decided; sent carries the txid; denied, dismissed, and broadcast_pending need \
        nothing from you; failed means the human may authorize it again.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn check_request(
        &self,
        Parameters(params): Parameters<CheckRequestParams>,
    ) -> Result<Json<RequestView>, ErrorData> {
        self.blocking(move |dir, network, agent, _providers| {
            let store = Store::open(dir.as_deref())?;
            let net_name = network_name(network);
            Ok(check_request_record(
                &store,
                net_name,
                &agent,
                &params.request_id,
            ))
        })
        .await
        .map(Json)
    }
}

/// The read-only request lookup behind `check_request`.
///
/// Lookup is by server request id only, scoped to this agent's own
/// directory, so another agent's record can never be exposed, and a
/// malformed id — an idempotency key included — is answered as a typed
/// not-found before anything touches the disk. Nothing here mutates: no
/// claim, no event, no reconciliation.
fn check_request_record(
    store: &Store,
    net_name: &'static str,
    agent: &str,
    raw_id: &str,
) -> RequestView {
    if !request::is_request_id(raw_id) {
        return RequestView::not_found(format!(
            "malformed request id {raw_id:?} — pass the request_id a request_send result \
             returned, not your idempotency_key"
        ));
    }
    match store.load_agent_request(net_name, agent, raw_id) {
        Ok(Some(record)) => RequestView::of(&record),
        Ok(None) => RequestView::not_found(format!(
            "no request {raw_id:?} recorded for agent {agent:?}"
        )),
        Err(e) => {
            let mut view = RequestView::error(
                "store_error",
                format!(
                    "cannot read request: {e:#} — ask the human to inspect it; do not file a replacement"
                ),
            );
            view.request_id = Some(raw_id.to_string());
            view
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
             human-created grant. Agents create requests, humans authorize requests, sats \
             executes requests. You can read the wallet and file send requests; you cannot \
             sign, approve, unlock, or broadcast, and you take no action after filing. All \
             amounts are integer satoshis. request_send() files a request, checked \
             deterministically against the grant's hard boundaries — budget, per-transaction \
             amount cap, fee cap, expiry, mode, recipient allowlist. The normal result is \
             status='pending_approval' with a request_id: relay it to your human, then \
             observe with check_request(request_id) — free, no side effects. status='denied' \
             names a grant boundary no approval lifts — report it once and stop; only the \
             human changing the grant can. request_send requires an idempotency_key: pick \
             a fresh one per send and reuse it on a retry, so a repeated call never files \
             twice. Use get_grant() to see the remaining budget before filing.",
            self.agent,
            network_name(self.network),
        ))
    }
}

#[cfg(test)]
mod receipt_tests {
    use super::*;

    const ID: &str = "r-0123456789abcdef0123456789abcdef";

    #[test]
    fn unreadable_request_is_a_store_error_with_its_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        let requests = store.agent_requests_dir("signet").join("alice");
        std::fs::create_dir_all(&requests).unwrap();
        let path = requests.join(format!("{ID}.json"));
        // Corrupt bytes and an unreadable record (directory) both remain errors.
        std::fs::write(&path, b"{").unwrap();
        for corrupt in [true, false] {
            if !corrupt {
                std::fs::remove_file(&path).unwrap();
                std::fs::create_dir(&path).unwrap();
            }
            let view = check_request_record(&store, "signet", "alice", ID);
            assert_eq!(view.status, "error");
            assert_eq!(view.error_code.as_deref(), Some("store_error"));
            assert_eq!(view.request_id.as_deref(), Some(ID));
            assert!(view.message.contains("do not file a replacement"));
            // Visibility remains scoped to the authenticated agent.
            assert_eq!(
                check_request_record(&store, "signet", "bob", ID).status,
                "not_found"
            );
        }
    }

    #[test]
    fn absent_and_malformed_requests_still_have_absence_results() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        for id in [ID, "../../other", "my-idempotency-key"] {
            let view = check_request_record(&store, "signet", "alice", id);
            assert_eq!(view.status, "not_found");
            assert!(view.error_code.is_none());
        }
        assert!(!store.agent_requests_dir("signet").exists());
    }
}
