//! The MCP tool surface — a shim over `satsd`.
//!
//! This process prepares transactions and broadcasts them. It holds no
//! key material and makes no authorization decision: `send` hands the
//! daemon a bearer token and an unsigned PSBT, and the daemon derives
//! what the transaction does, decides, signs, and persists.
//!
//! Denials are successful tool results with `status: "denied"` —
//! deterministic, machine-readable, and carrying the exact message the
//! agent should relay to its human.

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use sats_core::bitcoin::{Address, Network};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::commands::prepare;
use crate::config::{Config, network_name};
use crate::daemon::client::{Begun, Client};
use crate::daemon::protocol::{BroadcastOutcome, SendOutcome};
use crate::provider;
use crate::store::{Store, unix_now};
use crate::walletd;

/// Environment variable carrying the agent's bearer token, as printed
/// once by `sats agent grant`.
pub const TOKEN_ENV: &str = "SATS_AGENT_TOKEN";

#[derive(Clone)]
pub struct SatsMcp {
    dir: Option<PathBuf>,
    network: Network,
    agent: String,
    /// CLI --provider overrides the server was launched with; every tool
    /// call resolves providers the same way the CLI does.
    providers: Vec<provider::CliProvider>,
    /// The grant's bearer token. It names a policy the daemon enforces;
    /// it opens nothing and cannot recover the seed.
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

/// The daemon and the tool surface speak the same result vocabulary;
/// this is that vocabulary with a JSON schema attached.
impl From<SendOutcome> for SendResult {
    fn from(outcome: SendOutcome) -> SendResult {
        SendResult {
            status: outcome.status,
            txid: outcome.txid,
            amount_sat: outcome.amount_sat,
            fee_sat: outcome.fee_sat,
            total_sat: outcome.total_sat,
            remaining_budget_sat: outcome.remaining_budget_sat,
            reason: outcome.reason,
            message: outcome.message,
            request_id: outcome.request_id,
            error_code: outcome.error_code,
            via_approval: outcome.via_approval,
        }
    }
}

impl SendResult {
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
        let token = self.token.clone();
        self.blocking(move |dir, network, agent, providers| {
            let store = Store::open(dir.as_deref())?;
            let config = Config::load(&store)?;
            Ok(execute_send(
                &store, &config, network, &agent, &token, &providers, &params,
            ))
        })
        .await
        .map(Json)
    }
}

/// The agent spend path, as this process sees it.
///
/// Prepare, hand the daemon the PSBT, broadcast what it signed, report
/// back. Every decision — idempotency, the amount and fee, the grant
/// check, the budget draw — happens on the other side of the socket,
/// against a transaction the daemon derived for itself.
fn execute_send(
    store: &Store,
    config: &Config,
    network: Network,
    agent: &str,
    token: &str,
    providers: &[provider::CliProvider],
    params: &SendParams,
) -> SendResult {
    let net_name = network_name(network);

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

    let mut daemon = match Client::open(store, net_name) {
        Ok(client) => client,
        Err(e) => {
            return SendResult::op_error("daemon_unavailable", format!("{e:#}"));
        }
    };

    // Claim the request and run the amount-only precheck before spending
    // a chain sync on a request that cannot succeed.
    let request_id = match daemon.begin_send(
        token,
        agent,
        params.request_id.as_deref(),
        &recipient,
        params.amount_sat,
    ) {
        Ok(Begun::Proceed { request_id }) => request_id,
        Ok(Begun::Done(outcome)) => return outcome.into(),
        Err(e) => return SendResult::op_error("daemon_unavailable", format!("{e:#}")),
    };

    // Preparation is this side's job: syncing, guards, and fee estimation
    // all need the chain, which the daemon deliberately cannot reach.
    let prepared = (|| -> Result<_> {
        let services = provider::resolve(config, providers, network)?;
        let mut ctx = walletd::open(store, network)?;
        // prepare::build syncs internally and hard-fails on stale state;
        // the agent request form carries no safety bypasses.
        let request = prepare::PrepareRequest::for_agent(&params.address, params.amount_sat);
        let prepared = prepare::build(&mut ctx, &services, &request)?;
        ctx.persist()?;
        Ok((ctx, services, prepared))
    })();
    let (mut ctx, services, prepared) = match prepared {
        Ok(value) => value,
        Err(e) => {
            // Nothing is signed. Tell the daemon so the request record
            // closes out and a keyed retry can re-evaluate the same id.
            let message = format!("{e:#}");
            let outcome = daemon
                .finish(
                    token,
                    BroadcastOutcome::Failed {
                        message: message.clone(),
                    },
                )
                .unwrap_or_else(|_| SendOutcome::error(message).with_request(&request_id));
            return outcome.into();
        }
    };

    let signed =
        match daemon.authorize(token, &prepared.psbt().to_string(), prepared.excluded_utxos) {
            Ok(outcome) => outcome,
            Err(e) => return SendResult::op_error("daemon_unavailable", format!("{e:#}")),
        };
    if !signed.is_sent() {
        // Denied, or failed before a signature existed.
        return signed.into();
    }

    // The daemon persisted the finalized transaction before answering, so
    // the only copy is already durable. Read it back and broadcast.
    let txid = signed.txid.clone().unwrap_or_default();
    let broadcast = (|| -> Result<String> {
        let record = store.load_transaction(net_name, &txid)?;
        let tx = record.tx()?;
        Ok(services.broadcast(&mut ctx, &tx)?.to_string())
    })();
    let outcome = match broadcast {
        Ok(txid) => BroadcastOutcome::Broadcast { txid },
        Err(e) => BroadcastOutcome::Failed {
            message: format!("{e:#}"),
        },
    };
    match daemon.finish(token, outcome) {
        Ok(outcome) => outcome.into(),
        // The transaction may well be on the network; what failed is the
        // bookkeeping call. Say so rather than implying nothing happened.
        Err(e) => SendResult::op_error(
            "daemon_unavailable",
            format!(
                "transaction {txid} was signed and saved, but satsd could not be told the \
                 outcome: {e:#} — a human can check with: sats status {txid}"
            ),
        )
        .with_request(&request_id),
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
