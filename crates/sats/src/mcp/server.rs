//! The MCP tool surface. Denials are successful tool results with
//! `status: "denied"` — deterministic, machine-readable, and carrying the
//! exact message the agent should relay to its human.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use sats_core::authz::{Decision, SpendRequest, authorize_spend};
use sats_core::bitcoin::Network;
use sats_core::plan::PlanStatus;
use sats_core::signer::{LocalSigner, Signer};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::commands::plan;
use crate::config::{Config, network_name};
use crate::provider;
use crate::store::{Store, unix_now};
use crate::{keys, walletd};

#[derive(Clone)]
pub struct SatsMcp {
    dir: Option<PathBuf>,
    network: Network,
    agent: String,
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
    /// Denial code: expired, over_max_tx, over_max_fee, over_budget, revoked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
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
        }
    }
}

impl SatsMcp {
    pub fn new(dir: Option<PathBuf>, network: Network, agent: String) -> Self {
        SatsMcp {
            dir,
            network,
            agent,
            tool_router: Self::tool_router(),
        }
    }

    /// Run a blocking wallet operation off the async thread.
    async fn blocking<T, F>(&self, f: F) -> Result<T, ErrorData>
    where
        T: Send + 'static,
        F: FnOnce(Option<PathBuf>, Network, String) -> Result<T> + Send + 'static,
    {
        let (dir, network, agent) = (self.dir.clone(), self.network, self.agent.clone());
        tokio::task::spawn_blocking(move || f(dir, network, agent))
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
        self.blocking(|dir, network, _agent| {
            let store = Store::open(dir.as_deref())?;
            let config = Config::load(&store)?;
            let services = provider::resolve(&config, &[], network)?;
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
        self.blocking(|dir, network, _agent| {
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
        self.blocking(|dir, network, agent| {
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
                        "no active grant — ask the human to run: sats grant {agent} --budget <sats>"
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
        means human authorization is required, not that you should retry.")]
    async fn send(
        &self,
        Parameters(params): Parameters<SendParams>,
    ) -> Result<Json<SendResult>, ErrorData> {
        self.blocking(move |dir, network, agent| {
            let store = Store::open(dir.as_deref())?;
            let config = Config::load(&store)?;
            Ok(execute_send(&store, &config, network, &agent, &params))
        })
        .await
        .map(Json)
    }
}

/// The agent spend path: plan → authorize → reserve → sign → broadcast.
/// Every failure mode maps to a deterministic result, never a panic.
fn execute_send(
    store: &Store,
    config: &Config,
    network: Network,
    agent: &str,
    params: &SendParams,
) -> SendResult {
    let net_name = network_name(network);
    let now = unix_now();

    // Re-read the grant on every send so `sats revoke` takes effect
    // immediately, even mid-session.
    let mut grant = match store.load_grant(net_name, agent) {
        Ok(Some(g)) => g,
        Ok(None) => {
            return SendResult::denied(
                "revoked",
                format!(
                    "human authorization required: no active grant — ask the human to run: sats grant {agent} --budget <sats>"
                ),
            );
        }
        Err(e) => return SendResult::error(format!("{e:#}")),
    };

    // Pre-check on the amount alone (fee 0): an obviously over-limit
    // request is denied deterministically, before any network access.
    let precheck = SpendRequest {
        amount_sat: params.amount_sat,
        fee_sat: 0,
    };
    if let Decision::Deny(reason) = authorize_spend(&grant, &precheck, now) {
        return SendResult::denied(
            reason.code(),
            format!(
                "human authorization required: {}",
                reason.human().replace('\n', "; ")
            ),
        );
    }

    // Plan the transaction to learn the real fee before any decision.
    let planned = (|| -> Result<_> {
        let services = provider::resolve(config, &[], network)?;
        let mut ctx = walletd::open(store, network)?;
        // plan::build syncs internally and hard-fails on stale state.
        let plan = plan::build(&mut ctx, &services, &params.address, params.amount_sat, None)?;
        ctx.persist()?;
        Ok((ctx, services, plan))
    })();
    let (mut ctx, services, mut spend_plan) = match planned {
        Ok(v) => v,
        Err(e) => return SendResult::error(format!("{e:#}")),
    };

    let request = SpendRequest {
        amount_sat: spend_plan.amount_sat,
        fee_sat: spend_plan.fee_sat,
    };
    if let Decision::Deny(reason) = authorize_spend(&grant, &request, now) {
        return SendResult::denied(
            reason.code(),
            format!(
                "human authorization required: {}",
                reason.human().replace('\n', "; ")
            ),
        );
    }

    // Reserve and persist the draw-down BEFORE signing: once a signature
    // exists the money must be considered spent.
    if let Err(reason) = grant.reserve(&request, now) {
        return SendResult::denied(
            reason.code(),
            format!(
                "human authorization required: {}",
                reason.human().replace('\n', "; ")
            ),
        );
    }
    if let Err(e) = store.save_grant(net_name, &grant) {
        return SendResult::error(format!("cannot record spend: {e:#}"));
    }

    // Sign with the grant-wrapped seed.
    let signed = (|| -> Result<_> {
        let mnemonic = keys::unlock_grant(&grant, net_name)?;
        let mut psbt = spend_plan.psbt()?;
        let mut signer = LocalSigner::new(mnemonic, network);
        if !signer.sign(&mut psbt)? {
            return Err(anyhow!("signer produced an unfinalized transaction"));
        }
        Ok(psbt)
    })();
    let psbt = match signed {
        Ok(psbt) => psbt,
        Err(e) => {
            // No signature exists — safe to refund the reservation.
            grant.refund(&request);
            let _ = store.save_grant(net_name, &grant);
            return SendResult::error(format!("signing failed: {e:#}"));
        }
    };

    spend_plan.set_psbt(&psbt);
    spend_plan.status = PlanStatus::Signed;
    let plan_id = spend_plan.id.clone();
    if let Err(e) = store.save_plan(net_name, &spend_plan) {
        return SendResult::error(format!("cannot save signed plan: {e:#}"));
    }

    let tx = match spend_plan.tx() {
        Ok(tx) => tx,
        Err(e) => return SendResult::error(format!("{e:#}")),
    };
    match services.broadcast(&mut ctx, &tx) {
        Ok(txid) => {
            spend_plan.status = PlanStatus::Broadcast;
            let _ = store.save_plan(net_name, &spend_plan);
            SendResult::sent(
                txid.to_string(),
                request.amount_sat,
                request.fee_sat,
                grant.remaining_sat(),
            )
        }
        // Signed but not broadcast: budget stays reserved (the signed tx
        // is out of our hands), and a human can retry the saved plan.
        Err(e) => SendResult::error(format!(
            "broadcast failed after signing: {e:#} — budget reserved; a human can retry with: sats broadcast --plan {plan_id}"
        )),
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
             the message to your human instead of retrying. Use get_grant() to see the \
             remaining budget before sending.",
            self.agent,
            network_name(self.network),
        ))
    }
}
