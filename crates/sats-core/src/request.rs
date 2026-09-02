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
//! create ─► pending_approval ─approve─► executing ─► sent
//!                 │                        ├─► broadcast_pending  (signed; never signs again)
//!                 │                        ├─► failed             (nothing signed; refunded)
//!                 │                        └─► denied             (real-fee ladder)
//!                 ├─dismiss─► dismissed
//!                 └─(hard boundary at creation)─► denied
//! ```

use serde::{Deserialize, Serialize};

use crate::authz::DenyReason;

pub const REQUEST_FORMAT_VERSION: u32 = 1;

const fn request_format_version() -> u32 {
    REQUEST_FORMAT_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRequest {
    #[serde(default = "request_format_version")]
    pub format_version: u32,
    /// Record id and filename stem: `k-<client key>` for keyed requests,
    /// `r-<8 hex>` for keyless ones.
    pub id: String,
    pub network: String,
    pub agent: String,
    /// The idempotency key exactly as the client supplied it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_request_id: Option<String>,
    /// Normalized recipient address.
    pub recipient: String,
    pub amount_sat: u64,
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
/// request in either state must never be signed again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RequestState {
    /// Inside every grant boundary; awaiting a human decision.
    PendingApproval,
    /// A grant boundary refused it — at creation (fee unknown) or at
    /// execution (real fee). Terminal: no approval lifts a boundary.
    Denied { deny: DenyReason, at: u64 },
    /// The human declined. Terminal.
    Dismissed { at: u64 },
    /// A human authorized it and the budget is reserved; a signature is
    /// in progress. Carries what the reservation drew and which grant
    /// it drew from, so an interrupted execution can be reconciled.
    Executing {
        approved_at: u64,
        /// The fee the reservation was made with (amount is on the record).
        fee_sat: u64,
        /// `token_id` of the grant the reservation was drawn from.
        grant_token_id: String,
    },
    /// Signed and broadcast. Terminal.
    Sent { txid: String, fee_sat: u64, at: u64 },
    /// Signed, persisted, not broadcast. The reservation is final and
    /// the signer is never invoked again; only rebroadcasting the
    /// existing transaction settles it to `Sent`.
    BroadcastPending { txid: String, fee_sat: u64, at: u64 },
    /// Execution stopped before a durable signature existed. Nothing was
    /// signed and the reservation was refunded; a human may authorize
    /// again or dismiss.
    Failed { message: String, at: u64 },
}

impl RequestState {
    /// The wire status, matching the serde tag.
    pub fn status(&self) -> &'static str {
        match self {
            RequestState::PendingApproval => "pending_approval",
            RequestState::Denied { .. } => "denied",
            RequestState::Dismissed { .. } => "dismissed",
            RequestState::Executing { .. } => "executing",
            RequestState::Sent { .. } => "sent",
            RequestState::BroadcastPending { .. } => "broadcast_pending",
            RequestState::Failed { .. } => "failed",
        }
    }

    /// Whether a signed transaction exists for this request. Once true
    /// it stays true: such a request is never signed again and its
    /// reservation is never refunded.
    pub fn has_signature(&self) -> bool {
        matches!(
            self,
            RequestState::Sent { .. } | RequestState::BroadcastPending { .. }
        )
    }

    /// The signed transaction's id, when one exists.
    pub fn txid(&self) -> Option<&str> {
        match self {
            RequestState::Sent { txid, .. } | RequestState::BroadcastPending { txid, .. } => {
                Some(txid)
            }
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
        self.format_version == REQUEST_FORMAT_VERSION
    }

    /// Awaiting a human decision right now.
    pub fn is_pending_approval(&self) -> bool {
        matches!(self.state, RequestState::PendingApproval)
    }

    /// Whether a human may authorize execution from this state: a
    /// pending request, or one whose earlier execution stopped before
    /// any signature existed.
    pub fn is_approvable(&self) -> bool {
        matches!(
            self.state,
            RequestState::PendingApproval | RequestState::Failed { .. }
        )
    }

    /// Whether a human may dismiss it: anything still awaiting a decision
    /// or safely re-approvable.
    pub fn is_dismissable(&self) -> bool {
        self.is_approvable()
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
            client_request_id: Some("job-1".into()),
            recipient: "tb1pexample".into(),
            amount_sat: 25_000,
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
        assert!(json.get("state").is_none(), "the state is flattened");
        let back: AgentRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back.id, "k-job-1");
        assert_eq!(back.state, denied());
        assert!(!back.is_pending_approval());
        assert!(!back.is_approvable());
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
        let executing = RequestState::Executing {
            approved_at: 1,
            fee_sat: 100,
            grant_token_id: "t".into(),
        };
        assert!(sent.has_signature());
        assert!(pending_broadcast.has_signature());
        assert_eq!(pending_broadcast.txid(), Some("ab"));
        assert!(!failed.has_signature());
        assert!(!executing.has_signature());
        assert!(!denied().has_signature());
        assert!(!RequestState::PendingApproval.has_signature());
        // Only the no-signature stop is re-approvable.
        assert!(request(failed).is_approvable());
        assert!(!request(sent).is_approvable());
        assert!(!request(pending_broadcast).is_approvable());
        assert!(!request(executing).is_approvable());
        assert!(!request(RequestState::Dismissed { at: 2 }).is_approvable());
    }

    #[test]
    fn every_status_round_trips() {
        let states = [
            RequestState::PendingApproval,
            denied(),
            RequestState::Dismissed { at: 2 },
            RequestState::Executing {
                approved_at: 3,
                fee_sat: 100,
                grant_token_id: "t".into(),
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
            "recipient": "tb1p", "amount_sat": 1,
            "intent_digest": "d", "created_at": 0, "updated_at": 0,
            "status": "pending_approval",
        });
        let back: AgentRequest = serde_json::from_value(json).unwrap();
        assert!(back.version_supported());
    }

    /// Pre-release contract: a record in the retired outcome/approval
    /// shape does not parse, and is never migrated.
    #[test]
    fn retired_record_shape_does_not_parse() {
        let json = serde_json::json!({
            "id": "k-old", "network": "signet", "agent": "a",
            "recipient": "tb1p", "amount_sat": 1,
            "intent_digest": "d", "created_at": 0, "updated_at": 0,
            "outcome": {"status": "denied", "deny": {"reason": "ask_required"}, "resolved_at": 1},
        });
        assert!(serde_json::from_value::<AgentRequest>(json).is_err());
    }
}
