//! The MCP tools the relay serves to an agent.
//!
//! The same names, parameters, and result shapes as `sats agent serve`, so
//! an agent behaves the same whichever it reaches. The relay only seals
//! and queues: the human's device decides each filing against its own
//! grant, and `check_request` returns exactly the view the device posted.

use std::str::FromStr;
use std::sync::Arc;

use hyper::http::request::Parts;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use sats_core::bitcoin::{Address, Network};
use sats_core::relay::{self, Capability, FILING_VERSION, RelayedFiling, RelayedOperation};
use sats_core::request::{REQUEST_KEY_RULE, is_request_id, keyed_request_id, valid_request_key};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Connected;
use crate::mailbox::{Filed, Known, Mailbox, NewFiling};

#[derive(Deserialize, JsonSchema)]
pub struct CheckRequestParams {
    /// The request_id a filing tool returned: r- plus 32 hex characters.
    /// Not the idempotency_key you passed in.
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
    /// of filing a second one. Reusing a key for a different send is a
    /// typed error. This is not the request_id: the result carries that.
    pub idempotency_key: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestWrapParams {
    /// BTC to pay into the frBTC wrapper, in satoshis (at least 546).
    pub amount_sat: u64,
    /// 1-64 A-Za-z0-9_- characters. Reuse only to retrieve the same request.
    pub idempotency_key: String,
}

#[derive(Clone)]
pub struct RelayMcp {
    mailbox: Arc<Mailbox>,
    tool_router: ToolRouter<Self>,
}

fn view(value: Value) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::structured(value))
}

fn error(code: &str, message: impl Into<String>) -> Value {
    json!({"status": "error", "error_code": code, "message": message.into()})
}

fn pending(request_id: &str) -> Value {
    json!({
        "status": "pending_approval",
        "request_id": request_id,
        "message": "queued at the relay for the human's device, where the human reviews and \
                    approves it; poll check_request to observe the result, and do not file it \
                    again",
    })
}

fn network_of(capability: &Capability) -> Option<Network> {
    match capability.network.as_str() {
        "signet" => Some(Network::Signet),
        "testnet4" => Some(Network::Testnet4),
        "regtest" => Some(Network::Regtest),
        _ => None,
    }
}

/// The verified connection the HTTP layer attached, if it is live.
fn live(parts: &Parts) -> Result<&Connected, Value> {
    let connected = parts
        .extensions
        .get::<Connected>()
        .ok_or_else(|| error("unauthorized", "no connection on this request"))?;
    let capability = &connected.capability;
    if crate::now() >= capability.expires_at {
        return Err(error(
            "no_grant",
            format!(
                "the grant has expired — ask the human to run: sats agent grant {} --relay",
                capability.agent
            ),
        ));
    }
    Ok(connected)
}

/// What the device posted for a request, opened with this connection's
/// result key. The relay reads it only here, while serving the agent.
fn result_view(
    capability: &Capability,
    request_id: &str,
    status: &str,
    sealed: Option<Vec<u8>>,
) -> Value {
    let Some(sealed) = sealed else {
        return json!({
            "status": status,
            "request_id": request_id,
            "message": "the human's device refused this request; do not file it again",
        });
    };
    relay::open_result(&capability.result_key(), request_id, &sealed)
        .ok()
        .and_then(|plain| serde_json::from_slice(&plain).ok())
        .unwrap_or_else(|| {
            error(
                "relay_error",
                "the device's answer can't be read with this connection",
            )
        })
}

impl RelayMcp {
    pub fn new(mailbox: Arc<Mailbox>) -> Self {
        RelayMcp {
            mailbox,
            tool_router: Self::tool_router(),
        }
    }

    /// File, or return what an earlier filing with this key became.
    fn file(&self, connected: &Connected, key: &str, operation: RelayedOperation) -> Value {
        let capability = &connected.capability;
        if !valid_request_key(key) {
            return error("invalid_idempotency_key", REQUEST_KEY_RULE);
        }
        let request_id = keyed_request_id(&capability.grant_id, &capability.agent, key);
        let digest = operation.digest();
        let conflict = || {
            error(
                "idempotency_key_conflict",
                format!(
                    "idempotency_key was already used for a different request ({request_id}) — \
                     pick a fresh key"
                ),
            )
        };
        match self.mailbox.lookup(&request_id) {
            Ok(Some(Known::Result {
                digest: d,
                status,
                sealed,
                ..
            })) => {
                return if d == digest {
                    result_view(capability, &request_id, &status, sealed)
                } else {
                    conflict()
                };
            }
            Ok(Some(Known::Pending { digest: d, .. })) => {
                return if d == digest {
                    pending(&request_id)
                } else {
                    conflict()
                };
            }
            Ok(None) => {}
            Err(e) => return store_error(e),
        }
        let filing = RelayedFiling {
            v: FILING_VERSION,
            request_id: request_id.clone(),
            capability: connected.encoded.clone(),
            idempotency_key: key.to_string(),
            operation,
        };
        let sealed = relay::parse_public(&capability.human).and_then(|human| {
            relay::seal(
                &human,
                &relay::envelope_aad(&capability.human, &request_id),
                &serde_json::to_vec(&filing).expect("serializable"),
            )
        });
        let sealed = match sealed {
            Ok(sealed) => sealed,
            Err(e) => return error("relay_error", format!("cannot seal the request: {e}")),
        };
        match self.mailbox.file(&NewFiling {
            request_id: &request_id,
            human: &capability.human,
            agent: &capability.agent,
            digest: &digest,
            sealed: &sealed,
            now: crate::now(),
        }) {
            Ok(Filed::Stored) => pending(&request_id),
            Ok(Filed::MailboxFull) => error(
                "relay_mailbox_full",
                "the human's device has too many requests waiting — ask the human to review \
                 them before filing more",
            ),
            Err(e) => store_error(e),
        }
    }
}

fn store_error(e: anyhow::Error) -> Value {
    eprintln!("sats-relay: store error: {e:#}");
    error("store_error", "the relay's store failed — try again later")
}

#[tool_router]
impl RelayMcp {
    #[tool(
        description = "Return this connection's grant as the relay knows it: agent, network, \
        expiry. The human's device holds the authoritative grant (amount cap, recipient \
        allowlist, mode) and enforces it on every request.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn get_grant(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        let Some(Connected { capability, .. }) = parts.extensions.get::<Connected>() else {
            return view(error("unauthorized", "no connection on this request"));
        };
        view(json!({
            "active": crate::now() < capability.expires_at,
            "agent": capability.agent,
            "network": capability.network,
            "expires_at": capability.expires_at,
            "relayed": true,
        }))
    }

    #[tool(
        description = "File a request to send bitcoin. The relay queues it for the human's \
        device, which checks it against the grant (amount cap, expiry, mode, recipient \
        allowlist) and asks the human. You never approve, sign, or broadcast. The normal \
        result is status='pending_approval' with a request_id: tell your human, then observe \
        with check_request. A denial is final; do not file replacements.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn request_send(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<RequestSendParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let connected = match live(&parts) {
            Ok(connected) => connected,
            Err(refusal) => return view(refusal),
        };
        let capability = &connected.capability;
        let network = network_of(capability).expect("decode admits test networks only");
        let address = match Address::from_str(&params.address)
            .ok()
            .and_then(|a| a.require_network(network).ok())
        {
            Some(address) => address.to_string(),
            None => {
                return view(error(
                    "invalid_address",
                    format!(
                        "{:?} is not a {} address",
                        params.address, capability.network
                    ),
                ));
            }
        };
        view(self.file(
            connected,
            &params.idempotency_key,
            RelayedOperation::Send {
                address,
                amount_sat: params.amount_sat,
            },
        ))
    }

    #[tool(
        description = "File a request to wrap BTC into frBTC (32:0) for this wallet. You supply \
        only amount_sat and an idempotency_key: no destination, opcode, calldata, fee, keys, or \
        PSBT. The relay queues it for the human's device, which reads the contract facts, \
        checks the grant, and asks the human. You never approve, sign, or broadcast. Relay \
        pending_approval's request_id to the human, then only observe with check_request. sent \
        means the BTC transaction was broadcast, not that the frBTC is confirmed.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn request_wrap_frbtc(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<RequestWrapParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let connected = match live(&parts) {
            Ok(connected) => connected,
            Err(refusal) => return view(refusal),
        };
        if params.amount_sat < 546 {
            return view(error(
                "invalid_amount",
                "wrap amount must be at least 546 sat",
            ));
        }
        view(self.file(
            connected,
            &params.idempotency_key,
            RelayedOperation::FrbtcWrap {
                amount_sat: params.amount_sat,
            },
        ))
    }

    #[tool(
        description = "Check the state of one of your own requests by request_id. Read-only. \
        pending_approval means the human hasn't decided (or their device hasn't synced yet); \
        sent carries the txid; denied and dismissed are final; failed means the human may \
        authorize it again.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn check_request(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(params): Parameters<CheckRequestParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let connected = match live(&parts) {
            Ok(connected) => connected,
            Err(refusal) => return view(refusal),
        };
        let capability = &connected.capability;
        let not_found = || {
            json!({
                "status": "not_found",
                "message": format!(
                    "no request {:?} for agent {:?} — pass the request_id a filing tool \
                     returned, not your idempotency_key",
                    params.request_id, capability.agent
                ),
            })
        };
        if !is_request_id(&params.request_id) {
            return view(not_found());
        }
        // Scoped to this capability's mailbox and agent: another agent's
        // or device's request reads as not found.
        let mine =
            |human: &str, agent: &str| human == capability.human && agent == capability.agent;
        view(match self.mailbox.lookup(&params.request_id) {
            Ok(Some(Known::Result {
                human,
                agent,
                status,
                sealed,
                ..
            })) if mine(&human, &agent) => {
                result_view(capability, &params.request_id, &status, sealed)
            }
            Ok(Some(Known::Pending { human, agent, .. })) if mine(&human, &agent) => {
                pending(&params.request_id)
            }
            Ok(_) => not_found(),
            Err(e) => store_error(e),
        })
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RelayMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info =
            rmcp::model::Implementation::new("sats-relay", env!("CARGO_PKG_VERSION"));
        info.with_instructions(
            "sats relay: you reach a human's sats wallet through a relay. Agents create \
             requests, humans authorize requests, sats executes requests. You can file \
             bitcoin sends (request_send) and BTC-to-frBTC wraps (request_wrap_frbtc) and \
             observe them (check_request); you cannot sign, approve, unlock, or broadcast, and \
             you take no action after filing. Amounts are integer satoshis. Every filing needs \
             an idempotency_key: pick a fresh one per request and reuse it only on a retry. \
             The normal result is status='pending_approval': tell your human the request_id \
             and that they approve it on their device, then observe with check_request. \
             status='denied' names a grant boundary no approval lifts: report it once and stop.",
        )
    }
}
