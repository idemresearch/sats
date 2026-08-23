//! The causal record of the agent path.
//!
//! One event per state transition of an agent request — received,
//! decided, reserved, signed, broadcast, refunded — linked to its
//! request by id and to its exact meaning by intent digest. Events are
//! appended, never rewritten: the [`crate::request::AgentRequest`]
//! record holds current state, this log holds how it got there.

use serde::{Deserialize, Serialize};

use crate::authz::DenyReason;

pub const EVENT_FORMAT_VERSION: u32 = 1;

const fn event_format_version() -> u32 {
    EVENT_FORMAT_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    #[serde(default = "event_format_version")]
    pub format_version: u32,
    pub at: u64,
    pub network: String,
    pub agent: String,
    pub request_id: String,
    pub intent_digest: String,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventKind {
    RequestReceived {
        recipient: String,
        amount_sat: u64,
    },
    Denied {
        deny: DenyReason,
        /// "precheck" (offline, fee unknown) or "authorize" (real fee).
        stage: String,
    },
    Approved {
        max_fee_sat: u64,
        approval_expires_at: u64,
    },
    ApprovalRevoked,
    ApprovalConsumed {
        consumed_by_request: String,
    },
    Reserved {
        total_sat: u64,
        remaining_sat: u64,
        /// "grant" or "approval".
        via: String,
    },
    Refunded {
        total_sat: u64,
    },
    Signed {
        txid: String,
    },
    Broadcast {
        txid: String,
    },
    BroadcastFailed {
        txid: String,
        message: String,
    },
    Failed {
        message: String,
    },
    /// A keyed retry returned the recorded outcome; nothing executed.
    Replayed,
    /// A request key was reused for a different intent.
    Conflicted,
}

impl AgentEvent {
    pub fn version_supported(&self) -> bool {
        self.format_version == EVENT_FORMAT_VERSION
    }

    /// The serde tag, for rendering.
    pub fn kind_str(&self) -> &'static str {
        match self.kind {
            EventKind::RequestReceived { .. } => "request_received",
            EventKind::Denied { .. } => "denied",
            EventKind::Approved { .. } => "approved",
            EventKind::ApprovalRevoked => "approval_revoked",
            EventKind::ApprovalConsumed { .. } => "approval_consumed",
            EventKind::Reserved { .. } => "reserved",
            EventKind::Refunded { .. } => "refunded",
            EventKind::Signed { .. } => "signed",
            EventKind::Broadcast { .. } => "broadcast",
            EventKind::BroadcastFailed { .. } => "broadcast_failed",
            EventKind::Failed { .. } => "failed",
            EventKind::Replayed => "replayed",
            EventKind::Conflicted => "conflicted",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: EventKind) -> AgentEvent {
        AgentEvent {
            format_version: EVENT_FORMAT_VERSION,
            at: 1_500,
            network: "signet".into(),
            agent: "claude".into(),
            request_id: "k-job-1".into(),
            intent_digest: "d".repeat(64),
            kind,
        }
    }

    #[test]
    fn kind_flattens_into_the_event_object() {
        let e = event(EventKind::Reserved {
            total_sat: 4_781,
            remaining_sat: 45_219,
            via: "grant".into(),
        });
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["event"], "reserved");
        assert_eq!(json["total_sat"], 4_781);
        assert_eq!(json["request_id"], "k-job-1");
        assert_eq!(e.kind_str(), "reserved");
        let back: AgentEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back.kind_str(), "reserved");
    }

    #[test]
    fn unit_kinds_serialize_as_bare_tags() {
        let json = serde_json::to_value(event(EventKind::Replayed)).unwrap();
        assert_eq!(json["event"], "replayed");
        let back: AgentEvent = serde_json::from_value(json).unwrap();
        assert!(matches!(back.kind, EventKind::Replayed));
    }

    #[test]
    fn denial_events_nest_the_typed_reason() {
        let e = event(EventKind::Denied {
            deny: DenyReason::OverBudget {
                requested_sat: 20_000,
                remaining_sat: 4_588,
            },
            stage: "authorize".into(),
        });
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["event"], "denied");
        assert_eq!(json["deny"]["reason"], "over_budget");
        assert_eq!(json["stage"], "authorize");
    }

    #[test]
    fn kind_str_covers_every_variant() {
        let kinds = [
            event(EventKind::RequestReceived {
                recipient: "tb1p".into(),
                amount_sat: 1,
            }),
            event(EventKind::Approved {
                max_fee_sat: 1,
                approval_expires_at: 2,
            }),
            event(EventKind::ApprovalRevoked),
            event(EventKind::ApprovalConsumed {
                consumed_by_request: "k".into(),
            }),
            event(EventKind::Refunded { total_sat: 1 }),
            event(EventKind::Signed { txid: "t".into() }),
            event(EventKind::Broadcast { txid: "t".into() }),
            event(EventKind::BroadcastFailed {
                txid: "t".into(),
                message: "m".into(),
            }),
            event(EventKind::Failed {
                message: "m".into(),
            }),
            event(EventKind::Conflicted),
        ];
        for e in kinds {
            let json = serde_json::to_value(&e).unwrap();
            assert_eq!(json["event"], e.kind_str());
        }
    }

    #[test]
    fn version_defaults_on_old_lines() {
        let json = serde_json::json!({
            "at": 0, "network": "signet", "agent": "a",
            "request_id": "r", "intent_digest": "d",
            "event": "replayed",
        });
        let back: AgentEvent = serde_json::from_value(json).unwrap();
        assert!(back.version_supported());
    }
}
