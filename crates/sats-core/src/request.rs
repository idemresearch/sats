//! Durable agent request records.
//!
//! One record per agent send request, created at receipt and rewritten
//! through the lifecycle. It is the source of truth for idempotent
//! retries (the recorded outcome answers a repeated request key) and for
//! the human review queue (a denied request is what `sats agent approve`
//! acts on). The append-only event log in [`crate::event`] is the causal
//! audit beside it; the two share the request id.

use serde::{Deserialize, Serialize};

use crate::authz::{DenyReason, IntentApproval};

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RequestOutcome>,
    /// The one-time human approval for exactly this intent, when granted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<IntentApproval>,
    /// Set when a human dismissed the request from the review queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RequestOutcome {
    /// Signed and broadcast.
    Sent {
        txid: String,
        fee_sat: u64,
        resolved_at: u64,
    },
    /// The deterministic policy decision said no. Side-effect free: a
    /// retry under the same key re-evaluates instead of replaying.
    Denied { deny: DenyReason, resolved_at: u64 },
    /// An operational failure. With a txid, a signature exists (broadcast
    /// failed) and the outcome is replayed verbatim on retry; without
    /// one, nothing was signed and budget was refunded.
    Failed {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        txid: Option<String>,
        resolved_at: u64,
    },
}

impl AgentRequest {
    pub fn version_supported(&self) -> bool {
        self.format_version == REQUEST_FORMAT_VERSION
    }

    /// Awaiting human review: denied and not dismissed.
    pub fn is_pending(&self) -> bool {
        matches!(self.outcome, Some(RequestOutcome::Denied { .. })) && self.dismissed_at.is_none()
    }

    /// Whether executing this request left something irreversible — a
    /// signature. Such outcomes replay verbatim on a keyed retry; the
    /// rest re-evaluate.
    pub fn has_side_effect(&self) -> bool {
        matches!(
            self.outcome,
            Some(RequestOutcome::Sent { .. }) | Some(RequestOutcome::Failed { txid: Some(_), .. })
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(outcome: Option<RequestOutcome>) -> AgentRequest {
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
            outcome,
            approval: None,
            dismissed_at: None,
        }
    }

    fn denied() -> RequestOutcome {
        RequestOutcome::Denied {
            deny: DenyReason::OverMaxTx {
                requested_sat: 25_000,
                max_tx_sat: 10_000,
            },
            resolved_at: 1_001,
        }
    }

    #[test]
    fn json_round_trip_with_nested_denial() {
        let req = request(Some(denied()));
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["outcome"]["status"], "denied");
        assert_eq!(json["outcome"]["deny"]["reason"], "over_max_tx");
        assert!(json.get("approval").is_none(), "None fields are omitted");
        let back: AgentRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back.id, "k-job-1");
        assert!(back.is_pending());
    }

    #[test]
    fn side_effect_truth_table() {
        let sent = RequestOutcome::Sent {
            txid: "ab".into(),
            fee_sat: 100,
            resolved_at: 1,
        };
        let failed_signed = RequestOutcome::Failed {
            message: "broadcast failed".into(),
            txid: Some("ab".into()),
            resolved_at: 1,
        };
        let failed_unsigned = RequestOutcome::Failed {
            message: "signing failed".into(),
            txid: None,
            resolved_at: 1,
        };
        assert!(request(Some(sent)).has_side_effect());
        assert!(request(Some(failed_signed)).has_side_effect());
        assert!(!request(Some(failed_unsigned)).has_side_effect());
        assert!(!request(Some(denied())).has_side_effect());
        assert!(!request(None).has_side_effect());
    }

    #[test]
    fn pending_requires_denial_and_no_dismissal() {
        assert!(request(Some(denied())).is_pending());
        let mut dismissed = request(Some(denied()));
        dismissed.dismissed_at = Some(2_000);
        assert!(!dismissed.is_pending());
        assert!(!request(None).is_pending());
    }

    #[test]
    fn unknown_format_version_is_detected() {
        let mut req = request(None);
        req.format_version = 99;
        assert!(!req.version_supported());
        // Old files without the field default to the current version.
        let json = serde_json::json!({
            "id": "r-abc", "network": "signet", "agent": "a",
            "recipient": "tb1p", "amount_sat": 1,
            "intent_digest": "d", "created_at": 0, "updated_at": 0,
        });
        let back: AgentRequest = serde_json::from_value(json).unwrap();
        assert!(back.version_supported());
    }

    #[test]
    fn outcome_statuses_serialize_snake_case() {
        let sent = RequestOutcome::Sent {
            txid: "ab".into(),
            fee_sat: 100,
            resolved_at: 1,
        };
        assert_eq!(serde_json::to_value(&sent).unwrap()["status"], "sent");
        let failed = RequestOutcome::Failed {
            message: "m".into(),
            txid: None,
            resolved_at: 1,
        };
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!(json["status"], "failed");
        assert!(json.get("txid").is_none());
    }
}
