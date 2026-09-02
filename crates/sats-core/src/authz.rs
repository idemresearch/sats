//! The authorization engine: deterministic enforcement of human-set
//! authority for agent spending.
//!
//! [`evaluate_send`] is pure — no clock, no IO — and is the single
//! ladder every frontend decides through: the grant's hard boundaries
//! (expiry, observe mode, overflow, recipient rule, caps, budget), then
//! the terminal [`Decision::Ask`] — a valid proposal inside the grant,
//! which only a human may authorize. A grant bounds what an agent may
//! propose; the human's authorization executes one valid proposal
//! inside it and never exceeds it. The grant alone never allows a spend:
//! no agent-originated request may reach the signer without explicit
//! human authorization bound to that exact request. Callers reserve
//! budget *before* signing (a signed transaction is already spendable),
//! and refund only while no signature exists.

use serde::{Deserialize, Serialize};

use crate::fmt::format_sats;
use crate::token;

/// Current on-disk grant shape — the first released schema.
///
/// Records from a newer sats are refused by `Store::load_grant`: they
/// may carry restrictions this build cannot see, and ignoring them
/// would widen authority. Anything older is unreleased development
/// state and fails to parse; the fix is deleting the file and
/// re-granting. (The one special case: a pre-daemon development file
/// containing `wrapped_seed` is a seed disclosure and is refused with
/// wallet-rotation guidance rather than a parse error.)
pub const GRANT_FORMAT_VERSION: u32 = 1;

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
    pub agent: String,
    /// Network name, so a grant file copied across networks is inert.
    pub network: String,
    pub budget_sat: u64,
    /// Running total of amount + fee for every reserved spend.
    pub spent_sat: u64,
    /// Hard per-transaction amount cap (excluding fee). An amount above
    /// it is refused outright — no approval lifts a grant boundary; the
    /// only escalation is changing the grant. `None` leaves the budget
    /// as the only amount bound.
    pub max_tx_sat: Option<u64>,
    /// Hard per-transaction fee cap. Always present: a grant without one
    /// would let a single bad fee estimate burn the budget as miner
    /// fees, and with approvals bounded by the grant this is the one fee
    /// bound in the system.
    pub max_fee_sat: u64,
    pub created_at: u64,
    pub expires_at: u64,
    pub tx_count: u64,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpendRequest {
    pub amount_sat: u64,
    pub fee_sat: u64,
}

impl SpendRequest {
    /// What the spend draws from the budget: amount + fee. Saturating,
    /// for display and journaling; the authorization decision uses
    /// [`SpendRequest::checked_total_sat`] so an overflow denies rather
    /// than collapsing into a passable number.
    pub fn total_sat(&self) -> u64 {
        self.amount_sat.saturating_add(self.fee_sat)
    }

    /// The budget draw, or `None` when amount + fee overflows.
    pub fn checked_total_sat(&self) -> Option<u64> {
        self.amount_sat.checked_add(self.fee_sat)
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
    OverMaxFee {
        fee_sat: u64,
        max_fee_sat: u64,
    },
    OverBudget {
        requested_sat: u64,
        remaining_sat: u64,
    },
    /// Amount + fee overflows u64. Arithmetically absurd, so it fails
    /// closed instead of saturating into a number a budget could pass.
    AmountOverflow {
        amount_sat: u64,
        fee_sat: u64,
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

/// PURE. The per-transaction fee cap a grant gets when its creator does
/// not choose one: max(2% of budget, 1000 sat), clamped to the budget.
///
/// A grant with no fee cap lets a single bad fee estimate burn the whole
/// budget as miner fees; this keeps "budget + expiry" the only required
/// decisions while bounding that loss. Lifting the cap is an explicit
/// choice at creation.
pub fn default_max_fee_sat(budget_sat: u64) -> u64 {
    (budget_sat / 50).max(1_000).min(budget_sat)
}

/// PURE. The full verdict ladder for a send, recipient rule included.
/// This is the decision every surface enforces: at request creation
/// (fee unknown, so fee 0) and again at execution with the real fee.
///
/// Every rung is a grant boundary: expiry → observe mode → arithmetic
/// overflow → recipient rule → per-tx amount cap → per-tx fee cap →
/// budget. A proposal inside every boundary terminates in
/// [`Decision::Ask`]. The specific boundary answers before the terminal
/// ask, so a refusal names what was crossed.
pub fn evaluate_send(
    grant: &Grant,
    recipient: &str,
    req: &SpendRequest,
    now_unix: u64,
) -> Decision {
    // Expiry outranks everything: a dead grant reports dead.
    if grant.is_expired(now_unix) {
        return Decision::Deny(DenyReason::Expired {
            expired_at: grant.expires_at,
        });
    }
    if grant.mode == GrantMode::Observe {
        return Decision::Deny(DenyReason::ObserveOnly);
    }
    let Some(total_sat) = req.checked_total_sat() else {
        return Decision::Deny(DenyReason::AmountOverflow {
            amount_sat: req.amount_sat,
            fee_sat: req.fee_sat,
        });
    };
    if let Some(allowed) = &grant.allowed_recipients
        && !allowed.iter().any(|entry| entry == recipient)
    {
        return Decision::Deny(DenyReason::RecipientNotAllowed {
            recipient: recipient.to_string(),
        });
    }
    if let Some(max_tx_sat) = grant.max_tx_sat
        && req.amount_sat > max_tx_sat
    {
        return Decision::Deny(DenyReason::OverMaxTx {
            requested_sat: req.amount_sat,
            max_tx_sat,
        });
    }
    if req.fee_sat > grant.max_fee_sat {
        return Decision::Deny(DenyReason::OverMaxFee {
            fee_sat: req.fee_sat,
            max_fee_sat: grant.max_fee_sat,
        });
    }
    let remaining_sat = grant.remaining_sat();
    if total_sat > remaining_sat {
        return Decision::Deny(DenyReason::OverBudget {
            requested_sat: total_sat,
            remaining_sat,
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

    pub fn remaining_sat(&self) -> u64 {
        self.budget_sat.saturating_sub(self.spent_sat)
    }

    pub fn is_expired(&self, now_unix: u64) -> bool {
        now_unix >= self.expires_at
    }

    /// Draw a human-authorized spend down from the budget.
    ///
    /// The caller asserts the human authorization; this re-runs the full
    /// ladder — recipient rule included, with the real fee — so the draw
    /// can never exceed the grant: authorization executes a valid
    /// proposal inside the grant, never past it. Callers must persist
    /// the grant before signing.
    pub fn reserve_send(
        &mut self,
        recipient: &str,
        req: &SpendRequest,
        now_unix: u64,
    ) -> Result<(), DenyReason> {
        match evaluate_send(self, recipient, req, now_unix) {
            Decision::Ask => {}
            Decision::Deny(reason) => return Err(reason),
        }
        self.spent_sat = self.spent_sat.saturating_add(req.total_sat());
        self.tx_count = self.tx_count.saturating_add(1);
        Ok(())
    }

    /// Return a reservation whose signing did not produce a durable
    /// signature. Never refund after a signed transaction exists — it is
    /// spendable regardless of whether the broadcast succeeded. Both
    /// subtractions saturate, so a refund can never credit more than
    /// what stands reserved.
    pub fn refund(&mut self, req: &SpendRequest) {
        self.spent_sat = self.spent_sat.saturating_sub(req.total_sat());
        self.tx_count = self.tx_count.saturating_sub(1);
    }
}

impl DenyReason {
    pub fn code(&self) -> &'static str {
        match self {
            DenyReason::Expired { .. } => "expired",
            DenyReason::OverMaxTx { .. } => "over_max_tx",
            DenyReason::OverMaxFee { .. } => "over_max_fee",
            DenyReason::OverBudget { .. } => "over_budget",
            DenyReason::AmountOverflow { .. } => "amount_overflow",
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
            DenyReason::OverMaxFee {
                fee_sat,
                max_fee_sat,
            } => format!(
                "fee      {} sat\nmax fee  {} sat",
                format_sats(*fee_sat),
                format_sats(*max_fee_sat)
            ),
            DenyReason::OverBudget {
                requested_sat,
                remaining_sat,
            } => format!(
                "requested  {} sat (amount + fee)\nremaining  {} sat",
                format_sats(*requested_sat),
                format_sats(*remaining_sat)
            ),
            DenyReason::AmountOverflow {
                amount_sat,
                fee_sat,
            } => format!(
                "amount  {} sat\nfee     {} sat\namount + fee overflows",
                format_sats(*amount_sat),
                format_sats(*fee_sat)
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

    fn grant(budget: u64, spent: u64, max_tx: Option<u64>, max_fee: Option<u64>) -> Grant {
        let token = token::generate().unwrap();
        Grant {
            format_version: GRANT_FORMAT_VERSION,
            agent: "test".into(),
            network: "signet".into(),
            budget_sat: budget,
            spent_sat: spent,
            max_tx_sat: max_tx,
            max_fee_sat: max_fee.unwrap_or_else(|| default_max_fee_sat(budget)),
            created_at: 1_000,
            expires_at: 2_000,
            tx_count: 0,
            token_id: token.token_id,
            token_hash: token.token_hash,
            mode: GrantMode::Ask,
            allowed_recipients: None,
        }
    }

    const NOW: u64 = 1_500;
    const RECIPIENT: &str = "tb1ptestrecipient";

    fn req(amount: u64, fee: u64) -> SpendRequest {
        SpendRequest {
            amount_sat: amount,
            fee_sat: fee,
        }
    }

    fn decide(g: &Grant, amount: u64, fee: u64) -> Decision {
        evaluate_send(g, RECIPIENT, &req(amount, fee), NOW)
    }

    #[test]
    fn within_all_limits_terminates_in_ask() {
        let g = grant(50_000, 0, Some(10_000), Some(1_000));
        // The grant alone never authorizes: a valid in-envelope proposal
        // ends in the terminal ask, which only a human turns into an
        // execution.
        assert_eq!(decide(&g, 4_500, 281), Decision::Ask);
        assert!(decide(&g, 4_500, 281).is_ask());
    }

    #[test]
    fn denies_expired_even_when_within_budget() {
        let g = grant(50_000, 0, Some(10_000), None);
        assert_eq!(
            evaluate_send(&g, RECIPIENT, &req(1, 1), 2_000),
            Decision::Deny(DenyReason::Expired { expired_at: 2_000 }),
            "expiry is inclusive"
        );
        assert!(matches!(
            evaluate_send(&g, RECIPIENT, &req(1, 1), 5_000),
            Decision::Deny(DenyReason::Expired { .. })
        ));
    }

    #[test]
    fn expiry_trumps_every_other_reason() {
        let mut g = grant(1, 1, Some(1), Some(1));
        g.mode = GrantMode::Observe;
        g.allowed_recipients = Some(vec![]);
        assert!(matches!(
            evaluate_send(&g, RECIPIENT, &req(u64::MAX, u64::MAX), 5_000),
            Decision::Deny(DenyReason::Expired { .. })
        ));
    }

    #[test]
    fn max_tx_is_amount_only() {
        let g = grant(50_000, 0, Some(10_000), Some(5_000));
        assert_eq!(decide(&g, 10_000, 4_000), Decision::Ask, "cap is inclusive");
        assert_eq!(
            decide(&g, 10_001, 0),
            Decision::Deny(DenyReason::OverMaxTx {
                requested_sat: 10_001,
                max_tx_sat: 10_000
            })
        );
    }

    #[test]
    fn max_fee_is_fee_only() {
        let g = grant(50_000, 0, None, Some(1_000));
        assert_eq!(decide(&g, 40_000, 1_000), Decision::Ask, "cap is inclusive");
        assert_eq!(
            decide(&g, 100, 1_001),
            Decision::Deny(DenyReason::OverMaxFee {
                fee_sat: 1_001,
                max_fee_sat: 1_000
            })
        );
    }

    #[test]
    fn budget_counts_amount_plus_fee() {
        let g = grant(10_000, 0, None, Some(1_000));
        assert_eq!(
            decide(&g, 9_000, 1_000),
            Decision::Ask,
            "exactly the budget"
        );
        assert_eq!(
            decide(&g, 9_001, 1_000),
            Decision::Deny(DenyReason::OverBudget {
                requested_sat: 10_001,
                remaining_sat: 10_000
            })
        );
    }

    #[test]
    fn budget_accounts_for_prior_spending() {
        let g = grant(10_000, 6_100, None, None);
        assert_eq!(decide(&g, 3_800, 100), Decision::Ask);
        assert!(matches!(
            decide(&g, 3_900, 100),
            Decision::Deny(DenyReason::OverBudget {
                remaining_sat: 3_900,
                ..
            })
        ));
    }

    #[test]
    fn overspent_grant_denies_everything() {
        let g = grant(1_000, 1_000, None, None);
        assert!(matches!(
            decide(&g, 1, 0),
            Decision::Deny(DenyReason::OverBudget {
                remaining_sat: 0,
                ..
            })
        ));
        assert_eq!(g.remaining_sat(), 0);
    }

    #[test]
    fn zero_total_request_still_asks() {
        let g = grant(1_000, 0, None, None);
        assert_eq!(decide(&g, 0, 0), Decision::Ask);
    }

    /// Reserve as the human-authorized executor does: the ladder re-runs
    /// with the real fee, and only an in-envelope proposal draws.
    #[test]
    fn reserve_draws_down_and_recheck_denies() {
        let mut g = grant(10_000, 0, None, None);
        g.reserve_send(RECIPIENT, &req(6_000, 100), NOW).unwrap();
        assert_eq!(g.spent_sat, 6_100);
        assert_eq!(g.tx_count, 1);
        // A second spend that no longer fits reports the budget: the
        // specific boundary runs before the terminal ask.
        let err = g
            .reserve_send(RECIPIENT, &req(4_000, 100), NOW)
            .unwrap_err();
        assert_eq!(
            err,
            DenyReason::OverBudget {
                requested_sat: 4_100,
                remaining_sat: 3_900
            }
        );
        assert_eq!(g.spent_sat, 6_100, "failed reserve must not draw down");
    }

    #[test]
    fn refund_restores_reservation() {
        let mut g = grant(10_000, 0, None, None);
        let r = req(6_000, 100);
        g.reserve_send(RECIPIENT, &r, NOW).unwrap();
        g.refund(&r);
        assert_eq!(g.spent_sat, 0);
        assert_eq!(g.tx_count, 0);
        // The restored budget accepts a fresh reservation of the whole
        // amount.
        g.reserve_send(RECIPIENT, &req(9_900, 100), NOW).unwrap();
        assert_eq!(g.spent_sat, 10_000);
    }

    #[test]
    fn refund_saturates_at_zero() {
        let mut g = grant(10_000, 100, None, None);
        g.refund(&req(5_000, 5_000));
        assert_eq!(g.spent_sat, 0);
        assert_eq!(g.tx_count, 0);
    }

    #[test]
    fn overflowing_total_is_denied_not_saturated() {
        let mut g = grant(u64::MAX, 0, None, None);
        let r = req(u64::MAX, u64::MAX);
        // Display total still saturates; the decision does not.
        assert_eq!(r.total_sat(), u64::MAX);
        assert_eq!(r.checked_total_sat(), None);
        assert_eq!(
            evaluate_send(&g, RECIPIENT, &r, NOW),
            Decision::Deny(DenyReason::AmountOverflow {
                amount_sat: u64::MAX,
                fee_sat: u64::MAX,
            })
        );
        assert!(g.reserve_send(RECIPIENT, &r, NOW).is_err());
        assert_eq!(g.spent_sat, 0, "denied reserve must not draw down");
        assert_eq!(g.tx_count, 0);
        // The largest representable total is not overflow — it reaches
        // the terminal ask like any other in-envelope proposal.
        assert_eq!(decide(&g, u64::MAX, 0), Decision::Ask);
    }

    #[test]
    fn amount_overflow_serializes_with_tag() {
        let reason = DenyReason::AmountOverflow {
            amount_sat: u64::MAX,
            fee_sat: 1,
        };
        let json = serde_json::to_value(&reason).unwrap();
        assert_eq!(json["reason"], "amount_overflow");
        assert_eq!(json["amount_sat"], u64::MAX);
        assert_eq!(json["fee_sat"], 1);
        assert_eq!(reason.code(), "amount_overflow");
        assert!(reason.human().contains("overflows"));
    }

    #[test]
    fn tx_count_saturates_at_max() {
        let mut g = grant(10_000, 0, None, None);
        g.tx_count = u64::MAX;
        g.reserve_send(RECIPIENT, &req(1_000, 100), NOW).unwrap();
        assert_eq!(g.tx_count, u64::MAX);
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
    fn default_max_fee_floors_scales_and_clamps() {
        assert_eq!(default_max_fee_sat(10_000), 1_000, "floor wins");
        assert_eq!(default_max_fee_sat(50_000), 1_000, "2% below floor");
        assert_eq!(default_max_fee_sat(100_000), 2_000, "2% of budget");
        assert_eq!(default_max_fee_sat(1_000_000), 20_000);
        assert_eq!(default_max_fee_sat(500), 500, "never above the budget");
        assert_eq!(default_max_fee_sat(0), 0);
    }

    #[test]
    fn deny_reason_serializes_with_tag() {
        let reason = DenyReason::OverBudget {
            requested_sat: 20_000,
            remaining_sat: 4_588,
        };
        let json = serde_json::to_value(&reason).unwrap();
        assert_eq!(json["reason"], "over_budget");
        assert_eq!(json["requested_sat"], 20_000);
        assert_eq!(json["remaining_sat"], 4_588);
        assert_eq!(reason.code(), "over_budget");
    }

    #[test]
    fn grant_json_round_trip() {
        let g = grant(50_000, 12_412, Some(10_000), Some(1_000));
        let json = serde_json::to_string(&g).unwrap();
        let back: Grant = serde_json::from_str(&json).unwrap();
        assert_eq!(back.budget_sat, 50_000);
        assert_eq!(back.spent_sat, 12_412);
        assert_eq!(back.remaining_sat(), 37_588);
        assert_eq!(back.format_version, 1);
        assert_eq!(back.mode, GrantMode::Ask);
    }

    /// Pre-release contract: unreleased development state fails to parse
    /// rather than being migrated. A record without a mode, or carrying
    /// the removed `"auto"` value, is not honored in any form — the fix
    /// is deleting the file and granting again.
    #[test]
    fn unreleased_grant_state_fails_to_parse() {
        let mut json = serde_json::to_value(grant(50_000, 0, None, None)).unwrap();
        json.as_object_mut().unwrap().remove("mode");
        assert!(
            serde_json::from_value::<Grant>(json.clone()).is_err(),
            "a grant without a mode must not parse"
        );
        json["mode"] = serde_json::json!("auto");
        assert!(
            serde_json::from_value::<Grant>(json.clone()).is_err(),
            "the removed auto mode must not parse"
        );
        json["mode"] = serde_json::json!("ask");
        json.as_object_mut().unwrap().remove("max_fee_sat");
        assert!(
            serde_json::from_value::<Grant>(json).is_err(),
            "a grant without a fee cap must not parse"
        );
    }

    /// The core invariant, swept: no send request — routine, at a cap,
    /// zero, or enormous — is ever allowed by the ladder on its own. The
    /// only non-denial is the terminal ask.
    #[test]
    fn the_ladder_never_allows_a_spend() {
        let grants = [
            grant(50_000, 0, Some(10_000), Some(1_000)),
            grant(u64::MAX, 0, None, None),
            grant(1_000, 999, None, None),
        ];
        let requests = [
            req(0, 0),
            req(1, 0),
            req(10_000, 500),
            req(10_001, 0),
            req(u64::MAX, 0),
            req(u64::MAX, u64::MAX),
        ];
        for g in &grants {
            for r in &requests {
                assert!(
                    matches!(
                        evaluate_send(g, RECIPIENT, r, NOW),
                        Decision::Ask | Decision::Deny(_)
                    ),
                    "unexpected verdict for {r:?}"
                );
            }
        }
    }

    #[test]
    fn reserve_cannot_overspend_the_budget() {
        let mut g = grant(1_000, 0, None, None);
        // The budget is a grant boundary: human authorization does not
        // lift it, and a denied reserve draws nothing.
        assert!(matches!(
            g.reserve_send(RECIPIENT, &req(5_000, 100), NOW),
            Err(DenyReason::OverBudget { .. })
        ));
        assert_eq!(g.spent_sat, 0, "denied reserve must not draw down");
        g.reserve_send(RECIPIENT, &req(800, 100), NOW).unwrap();
        assert_eq!(g.spent_sat, 900);
        assert_eq!(g.remaining_sat(), 100);
    }

    #[test]
    fn observe_mode_denies_sends() {
        let mut g = grant(50_000, 0, None, None);
        g.mode = GrantMode::Observe;
        assert_eq!(decide(&g, 1, 1), Decision::Deny(DenyReason::ObserveOnly));
        assert!(g.reserve_send(RECIPIENT, &req(1, 1), NOW).is_err());
        assert_eq!(g.spent_sat, 0);
    }

    /// The intra-ladder order is deliberate: the specific boundary
    /// answers before the terminal ask, so a refusal names the boundary
    /// that was crossed instead of the generic ask.
    #[test]
    fn caps_speak_before_the_terminal_ask() {
        let g = grant(50_000, 0, Some(10_000), None);
        assert_eq!(
            decide(&g, 1_000, 10),
            Decision::Ask,
            "in-envelope: the routine ask"
        );
        assert!(
            matches!(
                decide(&g, 20_000, 10),
                Decision::Deny(DenyReason::OverMaxTx { .. })
            ),
            "over the cap: the specific hard refusal"
        );
    }

    /// Overflow answers before the recipient rule and the caps — an
    /// arithmetically absurd request never reads as a routine refusal —
    /// and the recipient rule answers before the caps.
    #[test]
    fn overflow_outranks_recipient_and_caps() {
        let mut g = grant(u64::MAX, 0, Some(10_000), None);
        g.allowed_recipients = Some(vec!["tb1pother".into()]);
        assert!(matches!(
            decide(&g, u64::MAX, u64::MAX),
            Decision::Deny(DenyReason::AmountOverflow { .. })
        ));
        assert!(matches!(
            decide(&g, 30_000, 10),
            Decision::Deny(DenyReason::RecipientNotAllowed { .. })
        ));
    }

    #[test]
    fn recipient_rule_is_a_hard_boundary() {
        let mut g = grant(50_000, 0, None, None);
        g.allowed_recipients = Some(vec![RECIPIENT.to_string(), "tb1pother".into()]);
        assert_eq!(
            decide(&g, 1_000, 10),
            Decision::Ask,
            "a listed recipient is a routine ask"
        );
        assert_eq!(
            evaluate_send(&g, "tb1pstranger", &req(1_000, 10), NOW),
            Decision::Deny(DenyReason::RecipientNotAllowed {
                recipient: "tb1pstranger".into()
            })
        );
        // An empty allowlist means no recipient may be proposed.
        g.allowed_recipients = Some(vec![]);
        assert!(matches!(
            decide(&g, 1_000, 10),
            Decision::Deny(DenyReason::RecipientNotAllowed { .. })
        ));
        assert!(g.reserve_send(RECIPIENT, &req(1_000, 10), NOW).is_err());
        assert_eq!(g.spent_sat, 0);
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
            (
                DenyReason::OverMaxFee {
                    fee_sat: 2,
                    max_fee_sat: 1,
                },
                "over_max_fee",
            ),
            (
                DenyReason::OverBudget {
                    requested_sat: 2,
                    remaining_sat: 1,
                },
                "over_budget",
            ),
            (
                DenyReason::AmountOverflow {
                    amount_sat: u64::MAX,
                    fee_sat: 1,
                },
                "amount_overflow",
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
        assert_eq!(cases.len(), 8, "every variant is listed");
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
        let mut g = grant(50_000, 0, Some(10_000), Some(1_000));
        g.allowed_recipients = Some(vec![RECIPIENT.to_string()]);
        let json = serde_json::to_value(&g).unwrap();
        assert_eq!(json["format_version"], 1);
        assert_eq!(json["mode"], "ask");
        assert_eq!(json["max_fee_sat"], 1_000);
        let back: Grant = serde_json::from_value(json).unwrap();
        assert_eq!(back.mode, GrantMode::Ask);
        assert_eq!(back.allowed_recipients, g.allowed_recipients);
        assert_eq!(back.max_fee_sat, 1_000);

        // Defaulted fields stay off the wire, so quiet grants stay small.
        let quiet = serde_json::to_value(grant(1, 0, None, None)).unwrap();
        assert!(quiet.get("allowed_recipients").is_none());
        assert_eq!(quiet["mode"], "ask");
    }

    /// Layer A of the signer-boundary family: every denial class leaves
    /// the budget untouched, so nothing downstream may sign.
    #[test]
    fn no_denied_send_draws_budget() {
        let expired = grant(50_000, 0, None, None);
        let observe = Grant {
            mode: GrantMode::Observe,
            ..grant(50_000, 0, None, None)
        };
        let capped = grant(50_000, 0, Some(10_000), None);
        let fee_capped = grant(50_000, 0, None, Some(100));
        let listed = Grant {
            allowed_recipients: Some(vec!["tb1pother".into()]),
            ..grant(50_000, 0, None, None)
        };
        let cases: Vec<(Grant, SpendRequest, u64)> = vec![
            (expired, req(1_000, 10), 2_000),
            (observe, req(1_000, 10), NOW),
            (capped, req(10_001, 10), NOW),
            (fee_capped, req(1_000, 101), NOW),
            (listed, req(1_000, 10), NOW),
            (grant(1_000, 0, None, None), req(1_000, 1), NOW),
            (grant(u64::MAX, 0, None, None), req(u64::MAX, u64::MAX), NOW),
        ];
        for (mut g, r, now) in cases {
            let spent_before = g.spent_sat;
            let tx_before = g.tx_count;
            let result = g.reserve_send(RECIPIENT, &r, now);
            assert!(result.is_err(), "expected a denial for {r:?} at {now}");
            assert_eq!(g.spent_sat, spent_before, "denied send drew budget");
            assert_eq!(g.tx_count, tx_before, "denied send counted a tx");
        }
    }
}
