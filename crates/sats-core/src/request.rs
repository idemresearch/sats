//! Durable agent request records: the first-class workflow object.
//!
//! An agent creates a request; a human authorizes or dismisses it; sats
//! executes it. One record per request, created at receipt and
//! rewritten through the lifecycle. It is the source of truth for the
//! human review queue and for what the agent observes. The append-only
//! event log in [`crate::event`] is the causal audit beside it; the two
//! share the request id.
//!
//! The state machine, with the signature boundary explicit:
//!
//! ```text
//! create ─► pending_approval ─approve─► signing ─► sent
//!                 │                        ├─► broadcast_pending  (signed; never signs again)
//!                 │                        ├─► unresolved         (a signature may exist; a human resolves)
//!                 │                        ├─► failed             (signer never invoked; re-approvable)
//!                 │                        └─► denied             (re-checked ladder, or the grant is gone)
//!                 ├─dismiss─► dismissed
//!                 └─(hard boundary at creation)─► denied
//! ```
//!
//! `signing` is persisted *before* the signer is invoked. Observing it on
//! disk therefore means the signer may have run: recovery never signs
//! again from that state.

use serde::{Deserialize, Serialize};

use crate::authz::DenyReason;

pub const REQUEST_FORMAT_VERSION: u32 = 1;
/// Older builds must never reinterpret a wrapping request as a BTC send.
pub const WRAP_REQUEST_FORMAT_VERSION: u32 = 3;
/// Asset transfers cannot be interpreted as BTC payments by older builds.
pub const ASSET_TRANSFER_REQUEST_FORMAT_VERSION: u32 = 4;

const fn request_format_version() -> u32 {
    REQUEST_FORMAT_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRequest {
    #[serde(default = "request_format_version")]
    pub format_version: u32,
    /// The server request id: `r-` plus 32 hex characters (128 bits),
    /// globally unique across agents. Derived deterministically from
    /// the grant, the agent, and the idempotency key, so a repeated
    /// filing resolves to the same record. This is the id humans
    /// review and approve; the idempotency key is only the agent's
    /// retry handle.
    pub id: String,
    pub network: String,
    pub agent: String,
    /// `grant_id` of the grant instance that created this request. A
    /// request executes only under that exact grant: revoking or
    /// re-issuing the grant makes every request filed under it
    /// non-executable.
    pub grant_id: String,
    /// The idempotency key exactly as the agent supplied it. Absent
    /// only on records not filed through `request_send`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Normalized recipient address.
    pub recipient: String,
    pub amount_sat: u64,
    #[serde(
        default,
        skip_serializing_if = "crate::intent::RequestOperation::is_send"
    )]
    pub operation: crate::intent::RequestOperation,
    /// Digest of the canonical [`crate::intent::SendIntent`].
    pub intent_digest: String,
    pub created_at: u64,
    pub updated_at: u64,
    /// Where the request is in its lifecycle. Flattened, so the record's
    /// JSON carries a top-level `status`.
    #[serde(flatten)]
    pub state: RequestState,
}

/// The lifecycle states. Whether a signed transaction exists is
/// structural: only `Sent` and `BroadcastPending` carry a txid, and a
/// request in either state must never be signed again. `Signing` and
/// `Unresolved` mean a signature *may* exist; they are never signed again
/// automatically either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RequestState {
    /// Inside every grant boundary; awaiting a human decision.
    PendingApproval,
    /// A grant boundary refused it — at creation, at the re-check under
    /// the grant lock at execution, or because the grant that created it
    /// is gone. Terminal: no approval lifts a boundary.
    Denied { deny: DenyReason, at: u64 },
    /// The human declined. Terminal.
    Dismissed { at: u64 },
    /// A human authorized it and the signer is about to be — or has
    /// been — invoked. Persisted before the signer runs, so on disk it is
    /// the irreversible boundary. Carries the fee the human reviewed
    /// (amount is on the record).
    Signing { approved_at: u64, fee_sat: u64 },
    /// Execution stopped at or after the signer and no persisted
    /// transaction settled it: a signature may exist. sats never signs
    /// again from here; a human resolves it. Carries the
    /// transaction id when it was known.
    Unresolved {
        at: u64,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        txid: Option<String>,
    },
    /// Signed and broadcast. Terminal.
    Sent { txid: String, fee_sat: u64, at: u64 },
    /// Signed, persisted, not broadcast. The signer is never invoked
    /// again; only rebroadcasting the
    /// existing transaction settles it to `Sent`.
    BroadcastPending { txid: String, fee_sat: u64, at: u64 },
    /// Execution stopped before `Signer::sign` was invoked — the audit
    /// log could not be written, or the signer could not be constructed
    /// Re-approvable only because it is
    /// provable that the signer never ran: nothing reported after the
    /// invocation is trusted to mean "no signature"; that is
    /// `Unresolved`. A human may authorize again or dismiss.
    Failed { message: String, at: u64 },
}

impl RequestState {
    /// The wire status, matching the serde tag.
    pub fn status(&self) -> &'static str {
        match self {
            RequestState::PendingApproval => "pending_approval",
            RequestState::Denied { .. } => "denied",
            RequestState::Dismissed { .. } => "dismissed",
            RequestState::Signing { .. } => "signing",
            RequestState::Unresolved { .. } => "unresolved",
            RequestState::Sent { .. } => "sent",
            RequestState::BroadcastPending { .. } => "broadcast_pending",
            RequestState::Failed { .. } => "failed",
        }
    }

    /// Whether a signed transaction is known to exist for this request.
    /// Once true it stays true: such a request is never signed again.
    pub fn has_signature(&self) -> bool {
        matches!(
            self,
            RequestState::Sent { .. } | RequestState::BroadcastPending { .. }
        )
    }

    /// Whether a signature may exist: the signer may have been invoked
    /// and nothing durable settled the outcome. Never sign again,
    /// automatically.
    pub fn may_have_signature(&self) -> bool {
        self.has_signature()
            || matches!(
                self,
                RequestState::Signing { .. } | RequestState::Unresolved { .. }
            )
    }

    /// The signed transaction's id, when one is known.
    pub fn txid(&self) -> Option<&str> {
        match self {
            RequestState::Sent { txid, .. } | RequestState::BroadcastPending { txid, .. } => {
                Some(txid)
            }
            RequestState::Unresolved { txid, .. } => txid.as_deref(),
            _ => None,
        }
    }

    /// The typed refusal, when the request was denied.
    pub fn deny_reason(&self) -> Option<&DenyReason> {
        match self {
            RequestState::Denied { deny, .. } => Some(deny),
            _ => None,
        }
    }
}

impl AgentRequest {
    pub fn version_supported(&self) -> bool {
        match self.operation {
            crate::intent::RequestOperation::Send => self.format_version == REQUEST_FORMAT_VERSION,
            crate::intent::RequestOperation::AssetTransfer { .. } => {
                self.format_version == ASSET_TRANSFER_REQUEST_FORMAT_VERSION
            }
            crate::intent::RequestOperation::FrbtcWrap { .. } => {
                self.format_version == WRAP_REQUEST_FORMAT_VERSION
            }
        }
    }

    pub fn expected_digest(&self) -> String {
        crate::intent::SendIntent {
            network: self.network.clone(),
            agent: self.agent.clone(),
            recipient: self.recipient.clone(),
            amount_sat: self.amount_sat,
        }
        .digest_for(&self.operation)
    }

    /// Awaiting a human decision right now.
    pub fn is_pending_approval(&self) -> bool {
        matches!(self.state, RequestState::PendingApproval)
    }

    /// Whether a human may authorize execution from this state: a
    /// pending request, or one whose earlier execution stopped before
    /// any signature could exist.
    pub fn is_approvable(&self) -> bool {
        matches!(
            self.state,
            RequestState::PendingApproval | RequestState::Failed { .. }
        )
    }

    /// Whether a human may dismiss it: anything awaiting a decision,
    /// re-approvable, or unresolved. Dismissing an unresolved request
    /// only closes the record: a signature may still exist, and the
    /// human checks the wallet for it.
    pub fn is_dismissable(&self) -> bool {
        self.is_approvable() || matches!(self.state, RequestState::Unresolved { .. })
    }

    pub fn status(&self) -> &'static str {
        self.state.status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(state: RequestState) -> AgentRequest {
        AgentRequest {
            format_version: REQUEST_FORMAT_VERSION,
            id: "k-job-1".into(),
            network: "signet".into(),
            agent: "claude".into(),
            grant_id: "g1".into(),
            idempotency_key: Some("job-1".into()),
            recipient: "tb1pexample".into(),
            amount_sat: 25_000,
            operation: crate::intent::RequestOperation::Send,
            intent_digest: "d".repeat(64),
            created_at: 1_000,
            updated_at: 1_000,
            state,
        }
    }

    fn denied() -> RequestState {
        RequestState::Denied {
            deny: DenyReason::OverMaxTx {
                requested_sat: 25_000,
                max_tx_sat: 10_000,
            },
            at: 1_001,
        }
    }

    #[test]
    fn status_flattens_into_the_record_with_nested_denial() {
        let req = request(denied());
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["status"], "denied");
        assert_eq!(json["deny"]["reason"], "over_max_tx");
        assert_eq!(json["grant_id"], "g1");
        assert!(json.get("state").is_none(), "the state is flattened");
        let back: AgentRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back.id, "k-job-1");
        assert_eq!(back.state, denied());
        assert!(!back.is_pending_approval());
        assert!(!back.is_approvable());
    }

    #[test]
    fn wrapping_requires_its_own_record_version_and_cannot_be_a_plain_send() {
        let mut req = request(RequestState::PendingApproval);
        assert!(req.version_supported());
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("operation").is_none());
        req.operation = crate::intent::RequestOperation::FrbtcWrap {
            contract: crate::intent::FrbtcContract {
                signer_address: "tb1pexample".into(),
                code_hash: "ab".repeat(32),
            },
        };
        assert!(!req.version_supported());
        req.format_version = WRAP_REQUEST_FORMAT_VERSION;
        assert!(req.version_supported());
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["operation"]["kind"], "frbtc_wrap");
        let back: AgentRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back.operation, req.operation);
        req.operation = crate::intent::RequestOperation::Send;
        assert!(!req.version_supported());
    }

    #[test]
    fn pending_serializes_as_a_bare_status() {
        let json = serde_json::to_value(request(RequestState::PendingApproval)).unwrap();
        assert_eq!(json["status"], "pending_approval");
        let back: AgentRequest = serde_json::from_value(json).unwrap();
        assert!(back.is_pending_approval());
        assert!(back.is_approvable());
        assert!(back.is_dismissable());
    }

    #[test]
    fn signature_boundary_is_structural() {
        let sent = RequestState::Sent {
            txid: "ab".into(),
            fee_sat: 100,
            at: 1,
        };
        let pending_broadcast = RequestState::BroadcastPending {
            txid: "ab".into(),
            fee_sat: 100,
            at: 1,
        };
        let failed = RequestState::Failed {
            message: "signing failed".into(),
            at: 1,
        };
        let signing = RequestState::Signing {
            approved_at: 1,
            fee_sat: 100,
        };
        let unresolved = RequestState::Unresolved {
            at: 1,
            message: "m".into(),
            txid: None,
        };
        assert!(sent.has_signature());
        assert!(pending_broadcast.has_signature());
        assert_eq!(pending_broadcast.txid(), Some("ab"));
        assert!(!failed.has_signature());
        assert!(!signing.has_signature());
        assert!(!unresolved.has_signature());
        assert!(!denied().has_signature());
        // Signing and unresolved may carry a signature: never signed
        // again automatically.
        assert!(signing.may_have_signature());
        assert!(unresolved.may_have_signature());
        assert!(sent.may_have_signature());
        assert!(!failed.may_have_signature());
        assert!(!RequestState::PendingApproval.may_have_signature());
        // Only the no-signature stop is re-approvable.
        assert!(request(failed).is_approvable());
        assert!(!request(sent).is_approvable());
        assert!(!request(pending_broadcast).is_approvable());
        assert!(!request(signing.clone()).is_approvable());
        assert!(!request(unresolved.clone()).is_approvable());
        assert!(!request(RequestState::Dismissed { at: 2 }).is_approvable());
        // Unresolved can be closed by a human, never re-signed.
        assert!(request(unresolved).is_dismissable());
        assert!(!request(signing).is_dismissable());
    }

    #[test]
    fn every_status_round_trips() {
        let states = [
            RequestState::PendingApproval,
            denied(),
            RequestState::Dismissed { at: 2 },
            RequestState::Signing {
                approved_at: 3,
                fee_sat: 100,
            },
            RequestState::Unresolved {
                at: 3,
                message: "m".into(),
                txid: Some("ab".into()),
            },
            RequestState::Sent {
                txid: "ab".into(),
                fee_sat: 100,
                at: 4,
            },
            RequestState::BroadcastPending {
                txid: "ab".into(),
                fee_sat: 100,
                at: 4,
            },
            RequestState::Failed {
                message: "m".into(),
                at: 5,
            },
        ];
        for state in states {
            let json = serde_json::to_value(request(state.clone())).unwrap();
            assert_eq!(json["status"], state.status());
            let back: AgentRequest = serde_json::from_value(json).unwrap();
            assert_eq!(back.state, state);
        }
    }

    #[test]
    fn unknown_format_version_is_detected() {
        let mut req = request(RequestState::PendingApproval);
        req.format_version = 99;
        assert!(!req.version_supported());
        // Files without the field default to the current version.
        let json = serde_json::json!({
            "id": "r-abc", "network": "signet", "agent": "a",
            "grant_id": "g1",
            "recipient": "tb1p", "amount_sat": 1,
            "intent_digest": "d", "created_at": 0, "updated_at": 0,
            "status": "pending_approval",
        });
        let back: AgentRequest = serde_json::from_value(json).unwrap();
        assert!(back.version_supported());
    }

    /// Pre-release contract: a record in a retired shape — the
    /// outcome/approval shape, or one not bound to a grant instance —
    /// does not parse, and is never migrated.
    #[test]
    fn retired_record_shapes_do_not_parse() {
        let json = serde_json::json!({
            "id": "k-old", "network": "signet", "agent": "a",
            "recipient": "tb1p", "amount_sat": 1,
            "intent_digest": "d", "created_at": 0, "updated_at": 0,
            "outcome": {"status": "denied", "deny": {"reason": "ask_required"}, "resolved_at": 1},
        });
        assert!(serde_json::from_value::<AgentRequest>(json).is_err());
        let unbound = serde_json::json!({
            "id": "k-old", "network": "signet", "agent": "a",
            "recipient": "tb1p", "amount_sat": 1,
            "intent_digest": "d", "created_at": 0, "updated_at": 0,
            "status": "pending_approval",
        });
        assert!(
            serde_json::from_value::<AgentRequest>(unbound).is_err(),
            "a request must name the grant that created it"
        );
    }
}
