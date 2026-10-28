//! The authorization engine: deterministic enforcement of the scope a
//! human sets for an agent's requests.
//!
//! [`evaluate_send`] is pure — no clock, no IO — and is the single
//! ladder every frontend decides through: the grant's hard boundaries
//! (expiry, observe mode, recipient rule, per-request amount cap), then
//! the terminal [`Decision::Ask`] — a valid proposal inside the grant,
//! which only a human may authorize. A grant bounds what an agent may
//! propose; the human's authorization executes one valid proposal
//! inside it and never exceeds it. The grant alone never allows a spend:
//! no agent-originated request may reach the signer without explicit
//! human authorization bound to that exact request.
//!
//! A grant is a credential and a scope, not an account: it holds no
//! budget and no spending ledger. The human reviews the real amount and
//! fee of every execution, so cumulative accounting would only repeat
//! that judgement.

use serde::{Deserialize, Serialize};

use crate::fmt::format_sats;
use crate::token;

/// A fresh grant identity: 128 random bits, hex. Non-secret, unique per
/// grant instance, and what every request created under the grant is
/// bound to. Distinct from `token_id`, which is a display prefix of the
/// bearer token's hash.
pub fn new_grant_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| format!("no entropy source: {e}"))?;
    Ok(hex::encode(bytes))
}

/// Current on-disk grant shape: a credential and a scope, with no
/// budget.
///
/// Records from a newer sats are refused by `Store::load_grant`: they
/// may carry restrictions this build cannot see, and ignoring them
/// would widen authority. Older records are unreleased development
/// state — versions 1 through 3 all carried a budget this build no
/// longer enforces — and are refused rather than migrated; the fix is
/// deleting the file and re-granting. Never reuse a retired number. (The one special case: a pre-daemon development
/// file containing `wrapped_seed` is a seed disclosure and is refused
/// with wallet-rotation guidance rather than a parse error.)
pub const GRANT_FORMAT_VERSION: u32 = 4;

const fn grant_format_version() -> u32 {
    GRANT_FORMAT_VERSION
}

/// How much standing authority a grant carries. The order is an
/// authority order — `ask > observe` — so tightening is cheap and
/// widening is a control-plane act.
///
/// There is no autonomous mode: an agent-originated spend can never be
/// authorized by the grant alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantMode {
    /// Every request waits for a human's authorization; the grant's
    /// boundaries decide what may be asked for at all.
    #[default]
    Ask,
    /// Read-only: no request can be created, and no approval can lift it.
    Observe,
}

impl GrantMode {
    pub fn as_str(self) -> &'static str {
        match self {
            GrantMode::Ask => "ask",
            GrantMode::Observe => "observe",
        }
    }

    /// Whether switching this mode to `next` widens authority. Widening
    /// requires the human's password; tightening never does.
    pub fn widens_to(self, next: GrantMode) -> bool {
        let rank = |mode: GrantMode| match mode {
            GrantMode::Observe => 0u8,
            GrantMode::Ask => 1,
        };
        rank(next) > rank(self)
    }
}

impl std::str::FromStr for GrantMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ask" => Ok(GrantMode::Ask),
            "observe" => Ok(GrantMode::Observe),
            other => Err(format!("unknown mode {other:?} (ask or observe)")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    #[serde(default = "grant_format_version")]
    pub format_version: u32,
    /// This grant instance's identity: 128 random bits, minted at
    /// creation. A request records the `grant_id` it was created under
    /// and executes only under that instance; a re-issued grant has a
    /// new one.
    pub grant_id: String,
    pub agent: String,
    /// Network name, so a grant file copied across networks is inert.
    pub network: String,
    /// Hard per-request amount cap (excluding fee). An amount above it
    /// is refused outright — no approval lifts a grant boundary; the
    /// only escalation is changing the grant. `None` leaves the amount
    /// to the human's review.
    pub max_tx_sat: Option<u64>,
    pub created_at: u64,
    pub expires_at: u64,
    /// Public identifier for this grant's bearer token: a prefix of
    /// `token_hash`, safe to log, display, and put in an error.
    pub token_id: String,
    /// SHA-256 of the bearer token, compared in constant time. The token
    /// itself is shown to the human once, at creation, and is never
    /// persisted by sats.
    ///
    /// This file holds no key material. Deleting it is revocation.
    pub token_hash: String,
    /// Standing authority mode. Required on disk: a record without one
    /// is unreleased development state and fails to parse.
    pub mode: GrantMode,
    /// Standing recipient allowlist, in the same normalized spelling the
    /// intent digest hashes. `None` means any recipient may be proposed;
    /// `Some(vec![])` means none may be.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_recipients: Option<Vec<String>>,
}

/// What a prepared transaction pays and costs, derived from the PSBT —
/// what the human reviews before authorizing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpendRequest {
    pub amount_sat: u64,
    pub fee_sat: u64,
}

impl SpendRequest {
    /// Amount + fee, saturating: for display only.
    pub fn total_sat(&self) -> u64 {
        self.amount_sat.saturating_add(self.fee_sat)
    }
}

/// Why a proposal is outside the grant. Every variant is a grant
/// boundary: no human approval lifts one, and the only escalation is
/// changing the grant itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum DenyReason {
    Expired {
        expired_at: u64,
    },
    OverMaxTx {
        requested_sat: u64,
        max_tx_sat: u64,
    },
    /// The grant is observe-only.
    ObserveOnly,
    /// The grant the request was filed under no longer exists: it was
    /// revoked, or replaced by a re-issued grant with a new token. A
    /// request is bound to the grant instance that created it and never
    /// executes under another.
    Revoked,
    /// The recipient is outside the grant's standing allowlist. Only
    /// editing the allowlist — never payment history — changes the list.
    RecipientNotAllowed {
        recipient: String,
    },
}

/// The verdict of the ladder. The grant alone never allows a spend:
/// a proposal inside every boundary is [`Decision::Ask`], and only a
/// human's authorization turns it into an execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// A valid proposal inside the grant, awaiting a human.
    Ask,
    /// Outside the grant. Terminal: no approval lifts it.
    Deny(DenyReason),
}

impl Decision {
    pub fn is_ask(&self) -> bool {
        matches!(self, Decision::Ask)
    }
}

/// The one agent-name rule, stated once for every surface's error text.
pub const AGENT_NAME_RULE: &str = "agent name must be 1-32 chars of a-z, 0-9, - or _";

/// PURE. Whether a string is a well-formed agent name.
///
/// Agent names become path components (grant files, request
/// directories), so this is a security check, not cosmetics: the only
/// names that may ever reach a path join are the ones this accepts.
/// Shared by the CLI, the MCP boundary, and the store.
pub fn valid_agent_name(agent: &str) -> bool {
    !agent.is_empty()
        && agent.len() <= 32
        && agent
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// PURE. The full verdict ladder for a proposal, recipient rule
/// included. This is the decision every surface enforces: at request
/// creation and again at execution, under the grant lock, so a revoked,
/// expired, or tightened grant refuses a request approved too late.
///
/// Every rung is a grant boundary: expiry → observe mode → recipient
/// rule → per-request amount cap. A proposal inside every boundary
/// terminates in [`Decision::Ask`]. The specific boundary answers before
/// the terminal ask, so a refusal names what was crossed. The fee is not
/// a rung: sats computes it, not the agent, and the human reviews it.
pub fn evaluate_send(grant: &Grant, recipient: &str, amount_sat: u64, now_unix: u64) -> Decision {
    // Expiry outranks everything: a dead grant reports dead.
    if grant.is_expired(now_unix) {
        return Decision::Deny(DenyReason::Expired {
            expired_at: grant.expires_at,
        });
    }
    if grant.mode == GrantMode::Observe {
        return Decision::Deny(DenyReason::ObserveOnly);
    }
    if let Some(allowed) = &grant.allowed_recipients
        && !allowed.iter().any(|entry| entry == recipient)
    {
        return Decision::Deny(DenyReason::RecipientNotAllowed {
            recipient: recipient.to_string(),
        });
    }
    if let Some(max_tx_sat) = grant.max_tx_sat
        && amount_sat > max_tx_sat
    {
        return Decision::Deny(DenyReason::OverMaxTx {
            requested_sat: amount_sat,
            max_tx_sat,
        });
    }
    // The terminal verdict: a valid proposal inside the grant still
    // waits for a human. The grant alone never authorizes a spend.
    Decision::Ask
}

impl Grant {
    /// Whether a presented bearer token authorizes this grant. Constant
    /// time in the token contents.
    pub fn authorizes(&self, token: &str) -> bool {
        token::verify(token, &self.token_hash)
    }

    pub fn is_expired(&self, now_unix: u64) -> bool {
        now_unix >= self.expires_at
    }
}

impl DenyReason {
    pub fn code(&self) -> &'static str {
        match self {
            DenyReason::Expired { .. } => "expired",
            DenyReason::OverMaxTx { .. } => "over_max_tx",
            DenyReason::ObserveOnly => "observe_only",
            DenyReason::Revoked => "revoked",
            DenyReason::RecipientNotAllowed { .. } => "recipient_not_allowed",
        }
    }

    /// The canonical refusal detail lines, shown under
    /// "outside the grant".
    pub fn human(&self) -> String {
        match self {
            DenyReason::Expired { .. } => "grant expired".to_string(),
            DenyReason::OverMaxTx {
                requested_sat,
                max_tx_sat,
            } => format!(
                "requested  {} sat\nmax tx     {} sat",
                format_sats(*requested_sat),
                format_sats(*max_tx_sat)
            ),
            DenyReason::ObserveOnly => "grant is observe-only".to_string(),
            DenyReason::Revoked => {
                "the grant this request was filed under was revoked or replaced".to_string()
            }
            DenyReason::RecipientNotAllowed { recipient } => {
                format!("recipient {recipient} is not on the grant's allowlist")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(max_tx: Option<u64>) -> Grant {
        let token = token::generate().unwrap();
        Grant {
            format_version: GRANT_FORMAT_VERSION,
            grant_id: new_grant_id().unwrap(),
            agent: "test".into(),
            network: "signet".into(),
            max_tx_sat: max_tx,
            created_at: 1_000,
            expires_at: 2_000,
            token_id: token.token_id,
            token_hash: token.token_hash,
            mode: GrantMode::Ask,
            allowed_recipients: None,
        }
    }

    const NOW: u64 = 1_500;
    const RECIPIENT: &str = "tb1ptestrecipient";

    fn decide(g: &Grant, amount: u64) -> Decision {
        evaluate_send(g, RECIPIENT, amount, NOW)
    }

    #[test]
    fn within_all_limits_terminates_in_ask() {
        let g = grant(Some(10_000));
        // The grant alone never authorizes: a valid in-scope proposal
        // ends in the terminal ask, which only a human turns into an
        // execution.
        assert_eq!(decide(&g, 4_500), Decision::Ask);
        assert!(decide(&g, 4_500).is_ask());
    }

    #[test]
    fn denies_expired() {
        let g = grant(Some(10_000));
        assert_eq!(
            evaluate_send(&g, RECIPIENT, 1, 2_000),
            Decision::Deny(DenyReason::Expired { expired_at: 2_000 }),
            "expiry is inclusive"
        );
        assert!(matches!(
            evaluate_send(&g, RECIPIENT, 1, 5_000),
            Decision::Deny(DenyReason::Expired { .. })
        ));
    }

    #[test]
    fn expiry_trumps_every_other_reason() {
        let mut g = grant(Some(1));
        g.mode = GrantMode::Observe;
        g.allowed_recipients = Some(vec![]);
        assert!(matches!(
            evaluate_send(&g, RECIPIENT, u64::MAX, 5_000),
            Decision::Deny(DenyReason::Expired { .. })
        ));
    }

    #[test]
    fn max_tx_is_inclusive() {
        let g = grant(Some(10_000));
        assert_eq!(decide(&g, 10_000), Decision::Ask, "cap is inclusive");
        assert_eq!(
            decide(&g, 10_001),
            Decision::Deny(DenyReason::OverMaxTx {
                requested_sat: 10_001,
                max_tx_sat: 10_000
            })
        );
    }

    #[test]
    fn uncapped_grant_leaves_the_amount_to_the_human() {
        let g = grant(None);
        assert_eq!(decide(&g, 0), Decision::Ask);
        assert_eq!(decide(&g, u64::MAX), Decision::Ask);
    }

    /// Pre-release contract: a grant without an identity does not parse.
    #[test]
    fn grant_without_identity_does_not_parse() {
        let mut json = serde_json::to_value(grant(None)).unwrap();
        json.as_object_mut().unwrap().remove("grant_id");
        assert!(serde_json::from_value::<Grant>(json).is_err());
    }

    #[test]
    fn agent_names_are_path_safe_or_rejected() {
        for good in ["claude", "agent-1", "a", "x_2", &"a".repeat(32)] {
            assert!(valid_agent_name(good), "{good:?} must be accepted");
        }
        for bad in [
            "",
            "../x",
            "a/b",
            "a\\b",
            "/etc",
            "a.b",
            "A",
            "café",
            "a b",
            "a\0b",
            &"a".repeat(33),
        ] {
            assert!(!valid_agent_name(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn deny_reason_serializes_with_tag() {
        let reason = DenyReason::OverMaxTx {
            requested_sat: 20_000,
            max_tx_sat: 10_000,
        };
        let json = serde_json::to_value(&reason).unwrap();
        assert_eq!(json["reason"], "over_max_tx");
        assert_eq!(json["requested_sat"], 20_000);
        assert_eq!(json["max_tx_sat"], 10_000);
        assert_eq!(reason.code(), "over_max_tx");
    }

    #[test]
    fn grant_json_round_trip() {
        let g = grant(Some(10_000));
        let json = serde_json::to_string(&g).unwrap();
        let back: Grant = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_tx_sat, Some(10_000));
        assert_eq!(back.format_version, GRANT_FORMAT_VERSION);
        assert_eq!(back.mode, GrantMode::Ask);
    }

    /// Pre-release contract: unreleased development state fails to parse
    /// rather than being migrated. A record without a mode, or carrying
    /// the removed `"auto"` value, is not honored in any form — the fix
    /// is deleting the file and granting again.
    #[test]
    fn unreleased_grant_state_fails_to_parse() {
        let mut json = serde_json::to_value(grant(None)).unwrap();
        json.as_object_mut().unwrap().remove("mode");
        assert!(
            serde_json::from_value::<Grant>(json.clone()).is_err(),
            "a grant without a mode must not parse"
        );
        json["mode"] = serde_json::json!("auto");
        assert!(
            serde_json::from_value::<Grant>(json).is_err(),
            "the removed auto mode must not parse"
        );
    }

    /// The core invariant, swept: no proposal — routine, at a cap, zero,
    /// or enormous — is ever allowed by the ladder on its own. The only
    /// non-denial is the terminal ask.
    #[test]
    fn the_ladder_never_allows_a_spend() {
        let grants = [grant(Some(10_000)), grant(None)];
        for g in &grants {
            for amount in [0, 1, 10_000, 10_001, u64::MAX] {
                assert!(
                    matches!(
                        evaluate_send(g, RECIPIENT, amount, NOW),
                        Decision::Ask | Decision::Deny(_)
                    ),
                    "unexpected verdict for {amount}"
                );
            }
        }
    }

    #[test]
    fn observe_mode_denies_sends() {
        let mut g = grant(None);
        g.mode = GrantMode::Observe;
        assert_eq!(decide(&g, 1), Decision::Deny(DenyReason::ObserveOnly));
    }

    /// The intra-ladder order is deliberate: the specific boundary
    /// answers before the terminal ask, so a refusal names the boundary
    /// that was crossed instead of the generic ask.
    #[test]
    fn caps_speak_before_the_terminal_ask() {
        let g = grant(Some(10_000));
        assert_eq!(
            decide(&g, 1_000),
            Decision::Ask,
            "in scope: the routine ask"
        );
        assert!(
            matches!(
                decide(&g, 20_000),
                Decision::Deny(DenyReason::OverMaxTx { .. })
            ),
            "over the cap: the specific hard refusal"
        );
    }

    /// The recipient rule answers before the amount cap.
    #[test]
    fn recipient_outranks_the_cap() {
        let mut g = grant(Some(10_000));
        g.allowed_recipients = Some(vec!["tb1pother".into()]);
        assert!(matches!(
            decide(&g, 30_000),
            Decision::Deny(DenyReason::RecipientNotAllowed { .. })
        ));
    }

    #[test]
    fn recipient_rule_is_a_hard_boundary() {
        let mut g = grant(None);
        g.allowed_recipients = Some(vec![RECIPIENT.to_string(), "tb1pother".into()]);
        assert_eq!(
            decide(&g, 1_000),
            Decision::Ask,
            "a listed recipient is a routine ask"
        );
        assert_eq!(
            evaluate_send(&g, "tb1pstranger", 1_000, NOW),
            Decision::Deny(DenyReason::RecipientNotAllowed {
                recipient: "tb1pstranger".into()
            })
        );
        // An empty allowlist means no recipient may be proposed.
        g.allowed_recipients = Some(vec![]);
        assert!(matches!(
            decide(&g, 1_000),
            Decision::Deny(DenyReason::RecipientNotAllowed { .. })
        ));
    }

    /// Every deny reason has a stable wire tag; the set is frozen here so
    /// adding one forces a decision about its code.
    #[test]
    fn deny_reasons_serialize_with_stable_tags() {
        let cases = [
            (DenyReason::Expired { expired_at: 9 }, "expired"),
            (
                DenyReason::OverMaxTx {
                    requested_sat: 2,
                    max_tx_sat: 1,
                },
                "over_max_tx",
            ),
            (DenyReason::ObserveOnly, "observe_only"),
            (DenyReason::Revoked, "revoked"),
            (
                DenyReason::RecipientNotAllowed {
                    recipient: "tb1p".into(),
                },
                "recipient_not_allowed",
            ),
        ];
        assert_eq!(cases.len(), 5, "every variant is listed");
        for (reason, code) in cases {
            let json = serde_json::to_value(&reason).unwrap();
            assert_eq!(json["reason"], code);
            assert_eq!(reason.code(), code);
            let back: DenyReason = serde_json::from_value(json).unwrap();
            assert_eq!(back, reason);
            assert!(!reason.human().is_empty());
        }
    }

    #[test]
    fn grant_mode_serializes_snake_case_and_orders_authority() {
        for (mode, s) in [(GrantMode::Ask, "ask"), (GrantMode::Observe, "observe")] {
            assert_eq!(serde_json::to_value(mode).unwrap(), s);
            assert_eq!(mode.as_str(), s);
            assert_eq!(s.parse::<GrantMode>().unwrap(), mode);
        }
        let err = "auto".parse::<GrantMode>().unwrap_err();
        assert!(err.contains("unknown mode"), "unexpected message: {err}");
        assert!("AUTO".parse::<GrantMode>().is_err());
        // Widening needs the password; tightening and staying put do not.
        assert!(GrantMode::Observe.widens_to(GrantMode::Ask));
        assert!(!GrantMode::Ask.widens_to(GrantMode::Observe));
        assert!(!GrantMode::Ask.widens_to(GrantMode::Ask));
        assert!(!GrantMode::Observe.widens_to(GrantMode::Observe));
    }

    #[test]
    fn grant_round_trips_with_every_field() {
        let mut g = grant(Some(10_000));
        g.allowed_recipients = Some(vec![RECIPIENT.to_string()]);
        let json = serde_json::to_value(&g).unwrap();
        assert_eq!(json["format_version"], GRANT_FORMAT_VERSION);
        assert_eq!(json["mode"], "ask");
        assert_eq!(json["max_tx_sat"], 10_000);
        assert!(json.get("budget_sat").is_none(), "a grant holds no budget");
        let back: Grant = serde_json::from_value(json).unwrap();
        assert_eq!(back.mode, GrantMode::Ask);
        assert_eq!(back.allowed_recipients, g.allowed_recipients);

        // Defaulted fields stay off the wire, so quiet grants stay small.
        let quiet = serde_json::to_value(grant(None)).unwrap();
        assert!(quiet.get("allowed_recipients").is_none());
        assert_eq!(quiet["mode"], "ask");
    }
}
