//! The authorization engine: deterministic enforcement of human-set
//! authority for agent spending.
//!
//! [`evaluate_send`] is pure — no clock, no IO — and is the single
//! ladder every frontend decides through: the grant's hard boundaries
//! (expiry, observe mode, intent authority, overflow, recipient rule,
//! caps, budget), then the terminal `ask_required` — the one refusal a
//! one-time human approval can lift. A grant bounds what an agent may
//! propose; an approval authorizes one valid proposal inside it and
//! never exceeds it. A send never allows on the grant alone: no
//! agent-originated spend may reach the signer without explicit,
//! one-time human authorization bound to that action. Callers reserve
//! budget *before* signing (a signed transaction is already spendable),
//! and refund only when signing fails.

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
    /// Every send terminates in a one-time human approval; the grant's
    /// boundaries decide what may be asked for at all.
    #[default]
    Ask,
    /// Read-only: no send can be authorized, and no approval can lift it.
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
    /// SHA-256 of the bearer token, compared in constant time by the
    /// signing daemon. The token itself is shown to the human once, at
    /// creation, and is never persisted by sats.
    ///
    /// This file holds no key material. Deleting it is revocation; the
    /// seed it authorizes spending from lives only in `satsd`'s memory.
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

/// A typed intent an agent asks the wallet to execute.
///
/// Send is the only executable intent today. The enum is the extension
/// point for venue intents: each variant carries the facts the budget
/// needs — worst-case sats leaving the wallet, plus the fee — never
/// protocol payloads, scripts, or provider data structures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentRequest {
    /// Transfer sats to an external recipient.
    Send(SpendRequest),
    /// Commit sats to a swap on a named venue. No grant can carry swap
    /// authority yet, so this always denies as not granted; the variant
    /// exists so the decision layer, denial code, and check order are
    /// fixed before any venue machinery lands.
    Swap {
        /// Local label of the venue the caller resolved.
        venue: String,
        /// Worst-case sats leaving the wallet, excluding the fee.
        give_sat: u64,
        fee_sat: u64,
    },
}

impl IntentRequest {
    pub fn kind(&self) -> &'static str {
        match self {
            IntentRequest::Send(_) => "send",
            IntentRequest::Swap { .. } => "swap",
        }
    }

    /// Worst-case sats leaving the wallet, excluding the fee.
    pub fn sats_out(&self) -> u64 {
        match self {
            IntentRequest::Send(spend) => spend.amount_sat,
            IntentRequest::Swap { give_sat, .. } => *give_sat,
        }
    }

    pub fn fee_sat(&self) -> u64 {
        match self {
            IntentRequest::Send(spend) => spend.fee_sat,
            IntentRequest::Swap { fee_sat, .. } => *fee_sat,
        }
    }

    /// What the intent draws from the budget: sats out + fee. Saturating,
    /// for display and journaling; decisions use
    /// [`IntentRequest::checked_total_sat`].
    pub fn total_sat(&self) -> u64 {
        self.sats_out().saturating_add(self.fee_sat())
    }

    /// The budget draw, or `None` when sats out + fee overflows.
    pub fn checked_total_sat(&self) -> Option<u64> {
        self.sats_out().checked_add(self.fee_sat())
    }
}

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
    IntentNotGranted {
        intent: String,
    },
    /// Amount + fee overflows u64. Arithmetically absurd, so it fails
    /// closed instead of saturating into a number a budget could pass.
    AmountOverflow {
        amount_sat: u64,
        fee_sat: u64,
    },
    /// The terminal verdict for a valid proposal inside the grant: the
    /// grant alone never authorizes a spend, so authorization requires a
    /// one-time human approval. The only approvable refusal.
    AskRequired,
    /// The grant is observe-only. Never approvable.
    ObserveOnly,
    /// The recipient is outside the grant's standing allowlist. A grant
    /// boundary: no approval lifts it, and only editing the allowlist —
    /// never payment history or an approval — changes the list.
    RecipientNotAllowed {
        recipient: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Unreachable for a spend: the ladder terminates every send in the
    /// approvable `ask_required`, so only a non-monetary decision could
    /// ever allow here. The variant stays for the type's completeness.
    Allow,
    Deny(DenyReason),
}

/// A one-time human authorization bound to an exact intent digest.
///
/// An approval authorizes one valid proposal *inside* the grant — it
/// lifts only the terminal `ask_required`, never a grant boundary. The
/// amount and recipient are pinned by the digest; the fee is bounded by
/// the grant's own fee cap at authorization time. It never survives
/// revocation (the grant file, and with it the signing key, is gone) and
/// never outranks grant expiry. Consumption is permanent: one approval
/// authorizes at most one signature, ever.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentApproval {
    /// Digest of the exact [`crate::intent::SendIntent`] the human saw.
    pub intent_digest: String,
    pub approved_at: u64,
    /// Inclusive, matching the grant convention: now >= expires_at is
    /// expired.
    pub expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_by_request: Option<String>,
}

impl IntentApproval {
    pub fn is_expired(&self, now_unix: u64) -> bool {
        now_unix >= self.expires_at
    }

    /// Whether this approval can authorize the given intent right now:
    /// same digest, never consumed, not expired.
    pub fn is_valid_for(&self, intent_digest: &str, now_unix: u64) -> bool {
        self.consumed_at.is_none()
            && !self.is_expired(now_unix)
            && self.intent_digest == intent_digest
    }
}

/// The decision of [`evaluate_send_with_approval`]: the only allow a
/// spend can reach rides a one-time human approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    AllowByApproval,
    Deny(DenyReason),
}

/// The one agent-name rule, stated once for every surface's error text.
pub const AGENT_NAME_RULE: &str = "agent name must be 1-32 chars of a-z, 0-9, - or _";

/// PURE. Whether a string is a well-formed agent name.
///
/// Agent names become path components (grant files, request
/// directories), so this is a security check, not cosmetics: the only
/// names that may ever reach a path join are the ones this accepts.
/// Shared by the CLI, the daemon socket boundary, and the store.
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

/// PURE. Send-shaped view of [`authorize_intent`], for callers that only
/// ever send. Both paths make the identical decision. Like
/// [`authorize_intent`], this cannot see the recipient — the full agent
/// decision is [`evaluate_send`].
pub fn authorize_spend(grant: &Grant, req: &SpendRequest, now_unix: u64) -> Decision {
    authorize_intent(grant, &IntentRequest::Send(*req), now_unix)
}

/// PURE. The full verdict ladder for a send, recipient rule included.
/// This is the decision the daemon enforces on every agent send.
pub fn evaluate_send(
    grant: &Grant,
    recipient: &str,
    req: &SpendRequest,
    now_unix: u64,
) -> Decision {
    ladder(grant, Some(recipient), &IntentRequest::Send(*req), now_unix)
}

/// PURE. [`evaluate_send`] minus the recipient rule, for callers that do
/// not have one (venue intents, the send-shaped compatibility views).
/// Everything else — the hard envelope and the rest of the ask band —
/// decides identically.
pub fn authorize_intent(grant: &Grant, req: &IntentRequest, now_unix: u64) -> Decision {
    ladder(grant, None, req, now_unix)
}

/// The single deterministic ladder behind every decision.
///
/// Every rung is a grant boundary, and none of them is liftable by an
/// approval: expiry → observe mode → intent authority → arithmetic
/// overflow → recipient rule (when the caller supplies one) → per-tx
/// amount cap → per-tx fee cap → budget. A proposal inside every
/// boundary terminates in `ask_required` — the one refusal a one-time
/// human approval lifts, so the only allow a spend can obtain is
/// [`ApprovalDecision::AllowByApproval`]. An approval authorizes a valid
/// proposal inside the grant; it never creates an exception to it.
fn ladder(grant: &Grant, recipient: Option<&str>, req: &IntentRequest, now_unix: u64) -> Decision {
    // Expiry outranks everything: a dead grant reports dead.
    if grant.is_expired(now_unix) {
        return Decision::Deny(DenyReason::Expired {
            expired_at: grant.expires_at,
        });
    }
    if grant.mode == GrantMode::Observe {
        return Decision::Deny(DenyReason::ObserveOnly);
    }
    // A grant carries send authority only; every other intent denies
    // until grants can carry typed rules for it.
    let req = match req {
        IntentRequest::Send(spend) => spend,
        other => {
            return Decision::Deny(DenyReason::IntentNotGranted {
                intent: other.kind().to_string(),
            });
        }
    };
    let Some(total_sat) = req.checked_total_sat() else {
        return Decision::Deny(DenyReason::AmountOverflow {
            amount_sat: req.amount_sat,
            fee_sat: req.fee_sat,
        });
    };
    if let (Some(recipient), Some(allowed)) = (recipient, &grant.allowed_recipients)
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
    // asks. The grant alone never authorizes a spend — mode is `Ask`
    // here (observe returned above), and there is no other mode.
    Decision::Deny(DenyReason::AskRequired)
}

/// PURE. [`evaluate_send`] extended with a one-time approval — the full
/// agent decision, as the daemon prechecks it.
pub fn evaluate_send_with_approval(
    grant: &Grant,
    recipient: &str,
    req: &SpendRequest,
    intent_digest: &str,
    approval: Option<&IntentApproval>,
    now_unix: u64,
) -> ApprovalDecision {
    lift_with_approval(
        evaluate_send(grant, recipient, req, now_unix),
        intent_digest,
        approval,
        now_unix,
    )
}

/// PURE. [`authorize_intent`] extended with a one-time approval.
pub fn authorize_intent_with_approval(
    grant: &Grant,
    req: &IntentRequest,
    intent_digest: &str,
    approval: Option<&IntentApproval>,
    now_unix: u64,
) -> ApprovalDecision {
    lift_with_approval(
        authorize_intent(grant, req, now_unix),
        intent_digest,
        approval,
        now_unix,
    )
}

/// The one approval-lifting rule, shared by both decision surfaces.
///
/// The plain decision runs first, unchanged, and only its terminal
/// `ask_required` can be lifted — by a valid approval (digest match,
/// unconsumed, unexpired) for exactly that intent. Every other refusal
/// is a grant boundary the approval cannot cross: an approval authorizes
/// a valid proposal inside the grant, never an exception to it.
fn lift_with_approval(
    decision: Decision,
    intent_digest: &str,
    approval: Option<&IntentApproval>,
    now_unix: u64,
) -> ApprovalDecision {
    let reason = match decision {
        // The ladder never allows a spend; treat a structural allow as
        // the terminal ask so the invariant holds even if a refactor
        // ever reintroduced one.
        Decision::Allow => DenyReason::AskRequired,
        Decision::Deny(reason) => reason,
    };
    if !reason.approvable() {
        return ApprovalDecision::Deny(reason);
    }
    match approval {
        Some(approval) if approval.is_valid_for(intent_digest, now_unix) => {
            ApprovalDecision::AllowByApproval
        }
        _ => ApprovalDecision::Deny(reason),
    }
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

    /// Re-check the full send ladder — recipient rule included — and
    /// draw the spend down from the budget. A send only ever reserves
    /// via an approval, and the ladder re-runs here with every boundary
    /// hard, so the draw can never exceed the budget: an approval
    /// authorizes a valid proposal inside the grant, never past it. The
    /// approval is marked consumed. Callers must persist the approval's
    /// holder before the grant, and the grant before signing.
    pub fn reserve_send(
        &mut self,
        recipient: &str,
        req: &SpendRequest,
        intent_digest: &str,
        approval: Option<&mut IntentApproval>,
        now_unix: u64,
    ) -> Result<(), DenyReason> {
        let decision = evaluate_send_with_approval(
            self,
            recipient,
            req,
            intent_digest,
            approval.as_deref(),
            now_unix,
        );
        match decision {
            ApprovalDecision::AllowByApproval => {
                if let Some(approval) = approval {
                    approval.consumed_at = Some(now_unix);
                }
            }
            ApprovalDecision::Deny(reason) => return Err(reason),
        }
        self.spent_sat = self.spent_sat.saturating_add(req.total_sat());
        self.tx_count = self.tx_count.saturating_add(1);
        Ok(())
    }

    /// Return a reservation whose signing failed. Never refund after a
    /// signature exists — a signed transaction is spendable regardless of
    /// whether the broadcast succeeded. Both subtractions saturate, so a
    /// refund can never credit more than what stands reserved.
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
            DenyReason::IntentNotGranted { .. } => "intent_not_granted",
            DenyReason::AmountOverflow { .. } => "amount_overflow",
            DenyReason::AskRequired => "ask_required",
            DenyReason::ObserveOnly => "observe_only",
            DenyReason::RecipientNotAllowed { .. } => "recipient_not_allowed",
        }
    }

    /// Whether a one-time human approval can lift this refusal. Only the
    /// terminal `ask_required` — a valid proposal inside the grant —
    /// qualifies; every other refusal is a grant boundary, and the only
    /// escalation is changing the grant itself. Every surface that
    /// offers, creates, or honors an approval must key off this one
    /// predicate.
    pub fn approvable(&self) -> bool {
        matches!(self, DenyReason::AskRequired)
    }

    /// The canonical refusal detail lines, shown under
    /// "human authorization required".
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
            DenyReason::IntentNotGranted { intent } => {
                format!("grant does not authorize {intent}")
            }
            DenyReason::AmountOverflow {
                amount_sat,
                fee_sat,
            } => format!(
                "amount  {} sat\nfee     {} sat\namount + fee overflows",
                format_sats(*amount_sat),
                format_sats(*fee_sat)
            ),
            DenyReason::AskRequired => {
                "every agent send needs a one-time human approval".to_string()
            }
            DenyReason::ObserveOnly => "grant is observe-only".to_string(),
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
    const DIGEST: &str = "d1";

    fn req(amount: u64, fee: u64) -> SpendRequest {
        SpendRequest {
            amount_sat: amount,
            fee_sat: fee,
        }
    }

    fn approval() -> IntentApproval {
        IntentApproval {
            intent_digest: DIGEST.into(),
            approved_at: NOW - 100,
            expires_at: 1_900,
            consumed_at: None,
            consumed_by_request: None,
        }
    }

    fn decide(
        g: &Grant,
        amount: u64,
        fee: u64,
        approval: Option<&IntentApproval>,
    ) -> ApprovalDecision {
        authorize_intent_with_approval(
            g,
            &IntentRequest::Send(req(amount, fee)),
            DIGEST,
            approval,
            NOW,
        )
    }

    #[test]
    fn within_all_limits_terminates_in_ask() {
        let g = grant(50_000, 0, Some(10_000), Some(1_000));
        // The grant alone never authorizes: a valid in-envelope proposal
        // ends in the approvable terminal ask.
        assert_eq!(
            authorize_spend(&g, &req(4_500, 281), NOW),
            Decision::Deny(DenyReason::AskRequired)
        );
        // Only a matching human approval turns it into an allow.
        assert_eq!(
            decide(&g, 4_500, 281, Some(&approval())),
            ApprovalDecision::AllowByApproval
        );
    }

    #[test]
    fn denies_expired_even_when_within_budget() {
        let g = grant(50_000, 0, None, None);
        // Expiry is inclusive: now == expires_at is expired.
        assert_eq!(
            authorize_spend(&g, &req(1, 1), 2_000),
            Decision::Deny(DenyReason::Expired { expired_at: 2_000 })
        );
        assert_eq!(
            authorize_spend(&g, &req(1, 1), 1_999),
            Decision::Deny(DenyReason::AskRequired),
            "unexpired: the terminal ask, not expiry"
        );
    }

    #[test]
    fn expiry_trumps_every_other_reason() {
        let g = grant(100, 0, Some(10), Some(1));
        // Over max-tx, max-fee, AND budget — but expired wins.
        assert!(matches!(
            authorize_spend(&g, &req(1_000, 1_000), 5_000),
            Decision::Deny(DenyReason::Expired { .. })
        ));
    }

    #[test]
    fn max_tx_is_amount_only() {
        let g = grant(50_000, 0, Some(10_000), None);
        // amount at the cap: a routine ask, even though amount+fee
        // exceeds the cap — the cap judges the amount alone.
        assert_eq!(
            authorize_spend(&g, &req(10_000, 500), NOW),
            Decision::Deny(DenyReason::AskRequired)
        );
        assert_eq!(
            authorize_spend(&g, &req(10_001, 0), NOW),
            Decision::Deny(DenyReason::OverMaxTx {
                requested_sat: 10_001,
                max_tx_sat: 10_000
            })
        );
    }

    #[test]
    fn max_fee_is_fee_only() {
        let g = grant(50_000, 0, None, Some(1_000));
        assert_eq!(
            authorize_spend(&g, &req(20_000, 1_000), NOW),
            Decision::Deny(DenyReason::AskRequired)
        );
        assert_eq!(
            authorize_spend(&g, &req(20_000, 1_001), NOW),
            Decision::Deny(DenyReason::OverMaxFee {
                fee_sat: 1_001,
                max_fee_sat: 1_000
            })
        );
    }

    #[test]
    fn budget_counts_amount_plus_fee() {
        let g = grant(10_000, 0, None, None);
        // Exactly the budget: a routine ask.
        assert_eq!(
            authorize_spend(&g, &req(9_500, 500), NOW),
            Decision::Deny(DenyReason::AskRequired)
        );
        // One sat over via the fee: the budget speaks.
        assert_eq!(
            authorize_spend(&g, &req(9_500, 501), NOW),
            Decision::Deny(DenyReason::OverBudget {
                requested_sat: 10_001,
                remaining_sat: 10_000
            })
        );
    }

    #[test]
    fn budget_accounts_for_prior_spending() {
        let g = grant(50_000, 45_412, None, None);
        assert_eq!(
            authorize_spend(&g, &req(4_500, 88), NOW),
            Decision::Deny(DenyReason::AskRequired)
        );
        assert_eq!(
            authorize_spend(&g, &req(4_500, 89), NOW),
            Decision::Deny(DenyReason::OverBudget {
                requested_sat: 4_589,
                remaining_sat: 4_588
            })
        );
    }

    #[test]
    fn overspent_grant_denies_everything() {
        // spent > budget (should be impossible, but must not underflow).
        let g = grant(1_000, 2_000, None, None);
        assert_eq!(g.remaining_sat(), 0);
        assert!(matches!(
            authorize_spend(&g, &req(0, 1), NOW),
            Decision::Deny(DenyReason::OverBudget { .. })
        ));
    }

    #[test]
    fn zero_total_request_still_asks() {
        let g = grant(1_000, 1_000, None, None);
        // Remaining is 0; a zero-sat request fits the budget (0 > 0 is
        // false) and still terminates in the ask — nothing auto-executes.
        assert_eq!(
            authorize_spend(&g, &req(0, 0), NOW),
            Decision::Deny(DenyReason::AskRequired)
        );
    }

    /// Reserve through a fresh matching approval — the only path a send
    /// can take, since the ladder never allows a spend on its own.
    fn reserve(g: &mut Grant, r: &SpendRequest, now: u64) -> Result<(), DenyReason> {
        let mut a = approval();
        g.reserve_send(RECIPIENT, r, DIGEST, Some(&mut a), now)
    }

    #[test]
    fn unapproved_reserve_terminates_in_ask_and_draws_nothing() {
        let mut g = grant(10_000, 0, None, None);
        let err = g
            .reserve_send(RECIPIENT, &req(1_000, 100), DIGEST, None, NOW)
            .unwrap_err();
        assert_eq!(err, DenyReason::AskRequired);
        assert_eq!(g.spent_sat, 0, "a denied send draws no budget");
        assert_eq!(g.tx_count, 0);
    }

    #[test]
    fn reserve_draws_down_and_recheck_denies() {
        let mut g = grant(10_000, 0, None, None);
        reserve(&mut g, &req(6_000, 100), NOW).unwrap();
        assert_eq!(g.spent_sat, 6_100);
        assert_eq!(g.tx_count, 1);
        // A second spend that no longer fits reports the budget, not the
        // terminal ask — the specific reason runs first.
        let err = g
            .reserve_send(RECIPIENT, &req(4_000, 100), DIGEST, None, NOW)
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
        reserve(&mut g, &r, NOW).unwrap();
        g.refund(&r);
        assert_eq!(g.spent_sat, 0);
        assert_eq!(g.tx_count, 0);
        // The restored budget accepts a fresh approved reservation of
        // the whole amount.
        reserve(&mut g, &req(9_900, 100), NOW).unwrap();
        assert_eq!(g.spent_sat, 10_000);
    }

    #[test]
    fn overflowing_total_is_denied_not_saturated() {
        let mut g = grant(u64::MAX, 0, None, None);
        let r = req(u64::MAX, u64::MAX);
        // Display total still saturates; the decision does not.
        assert_eq!(r.total_sat(), u64::MAX);
        assert_eq!(r.checked_total_sat(), None);
        assert_eq!(
            authorize_spend(&g, &r, NOW),
            Decision::Deny(DenyReason::AmountOverflow {
                amount_sat: u64::MAX,
                fee_sat: u64::MAX,
            })
        );
        assert!(reserve(&mut g, &r, NOW).is_err());
        assert_eq!(g.spent_sat, 0, "denied reserve must not draw down");
        assert_eq!(g.tx_count, 0);
        // The largest representable total is not overflow — it reaches
        // the terminal ask like any other in-envelope proposal.
        assert_eq!(
            authorize_spend(&g, &req(u64::MAX, 0), NOW),
            Decision::Deny(DenyReason::AskRequired)
        );
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
    fn approval_never_lifts_amount_overflow() {
        let g = grant(u64::MAX, 0, None, None);
        let a = approval();
        assert!(matches!(
            decide(&g, u64::MAX, u64::MAX, Some(&a)),
            ApprovalDecision::Deny(DenyReason::AmountOverflow { .. })
        ));
    }

    #[test]
    fn tx_count_saturates_at_max() {
        let mut g = grant(10_000, 0, None, None);
        g.tx_count = u64::MAX;
        reserve(&mut g, &req(1_000, 100), NOW).unwrap();
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

    fn swap(give: u64, fee: u64) -> IntentRequest {
        IntentRequest::Swap {
            venue: "testvenue".into(),
            give_sat: give,
            fee_sat: fee,
        }
    }

    #[test]
    fn ungranted_intent_is_denied_even_within_budget() {
        let g = grant(50_000, 0, None, None);
        assert_eq!(
            authorize_intent(&g, &swap(1_000, 100), NOW),
            Decision::Deny(DenyReason::IntentNotGranted {
                intent: "swap".into()
            })
        );
    }

    #[test]
    fn expiry_trumps_intent_authority() {
        let g = grant(50_000, 0, None, None);
        assert!(matches!(
            authorize_intent(&g, &swap(1_000, 100), 5_000),
            Decision::Deny(DenyReason::Expired { .. })
        ));
    }

    #[test]
    fn send_intent_and_authorize_spend_agree() {
        let g = grant(50_000, 45_412, Some(10_000), Some(1_000));
        for (amount, fee) in [(4_500, 88), (4_500, 89), (10_001, 0), (0, 1_001), (0, 0)] {
            let r = req(amount, fee);
            assert_eq!(
                authorize_spend(&g, &r, NOW),
                authorize_intent(&g, &IntentRequest::Send(r), NOW),
                "decisions diverged for amount {amount} fee {fee}"
            );
        }
    }

    #[test]
    fn evaluate_send_agrees_with_authorize_spend_without_an_allowlist() {
        let g = grant(50_000, 4_781, Some(10_000), Some(1_000));
        for (amount, fee) in [(4_500, 281), (10_001, 0), (0, 1_001), (45_000, 300), (0, 0)] {
            assert_eq!(
                authorize_spend(&g, &req(amount, fee), NOW),
                evaluate_send(&g, RECIPIENT, &req(amount, fee), NOW),
                "diverged for amount {amount} fee {fee}"
            );
        }
    }

    #[test]
    fn intent_accessors_cover_every_kind() {
        let send = IntentRequest::Send(req(9_500, 500));
        assert_eq!(send.kind(), "send");
        assert_eq!(send.sats_out(), 9_500);
        assert_eq!(send.fee_sat(), 500);
        assert_eq!(send.total_sat(), 10_000);

        let s = swap(u64::MAX, 1);
        assert_eq!(s.kind(), "swap");
        assert_eq!(s.sats_out(), u64::MAX);
        assert_eq!(s.total_sat(), u64::MAX, "budget draw must saturate");
    }

    #[test]
    fn intent_not_granted_serializes_with_tag() {
        let reason = DenyReason::IntentNotGranted {
            intent: "swap".into(),
        };
        let json = serde_json::to_value(&reason).unwrap();
        assert_eq!(json["reason"], "intent_not_granted");
        assert_eq!(json["intent"], "swap");
        assert_eq!(reason.code(), "intent_not_granted");
        assert_eq!(reason.human(), "grant does not authorize swap");
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

    /// The one lift there is: a valid proposal inside every grant
    /// boundary, paired with a valid approval for exactly that intent.
    #[test]
    fn approval_lifts_only_the_terminal_ask() {
        let mut g = grant(50_000, 0, Some(10_000), Some(1_000));
        let mut a = approval();
        assert_eq!(
            decide(&g, 1_000, 100, Some(&a)),
            ApprovalDecision::AllowByApproval
        );
        g.reserve_send(RECIPIENT, &req(1_000, 100), DIGEST, Some(&mut a), NOW)
            .unwrap();
        assert_eq!(a.consumed_at, Some(NOW), "the approval is spent");
        assert_eq!(g.spent_sat, 1_100);
        assert_eq!(g.tx_count, 1);
    }

    /// A grant bounds what an agent may propose; human approval does not
    /// override the grant. Every boundary holds with a valid approval in
    /// hand, and each denial names its boundary, not the ask.
    #[test]
    fn approval_never_overrides_the_grant() {
        let a = approval();
        // Over the amount cap.
        let g = grant(50_000, 0, Some(10_000), None);
        assert!(matches!(
            decide(&g, 20_000, 100, Some(&a)),
            ApprovalDecision::Deny(DenyReason::OverMaxTx { .. })
        ));
        // Over the fee cap.
        let g = grant(50_000, 0, None, Some(100));
        assert!(matches!(
            decide(&g, 1_000, 500, Some(&a)),
            ApprovalDecision::Deny(DenyReason::OverMaxFee { .. })
        ));
        // Over the budget.
        let g = grant(1_000, 0, None, None);
        assert!(matches!(
            decide(&g, 5_000, 100, Some(&a)),
            ApprovalDecision::Deny(DenyReason::OverBudget { .. })
        ));
        // Off the recipient allowlist.
        let mut g = grant(50_000, 0, None, None);
        g.allowed_recipients = Some(vec!["tb1pother".into()]);
        assert!(matches!(
            evaluate_send_with_approval(&g, RECIPIENT, &req(1_000, 100), DIGEST, Some(&a), NOW),
            ApprovalDecision::Deny(DenyReason::RecipientNotAllowed { .. })
        ));
        // Observe-only.
        let mut g = grant(50_000, 0, None, None);
        g.mode = GrantMode::Observe;
        assert!(matches!(
            evaluate_send_with_approval(&g, RECIPIENT, &req(1_000, 100), DIGEST, Some(&a), NOW),
            ApprovalDecision::Deny(DenyReason::ObserveOnly)
        ));
    }

    #[test]
    fn approval_never_overrides_expiry() {
        let g = grant(50_000, 0, Some(10), None);
        let a = IntentApproval {
            expires_at: 10_000,
            ..approval()
        };
        assert!(matches!(
            authorize_intent_with_approval(
                &g,
                &IntentRequest::Send(req(1, 1)),
                DIGEST,
                Some(&a),
                5_000, // grant expired at 2_000
            ),
            ApprovalDecision::Deny(DenyReason::Expired { .. })
        ));
    }

    #[test]
    fn approval_never_overrides_intent_authority() {
        let g = grant(50_000, 0, None, None);
        let a = approval();
        assert!(matches!(
            authorize_intent_with_approval(&g, &swap(1_000, 100), DIGEST, Some(&a), NOW),
            ApprovalDecision::Deny(DenyReason::IntentNotGranted { .. })
        ));
    }

    #[test]
    fn spent_or_stale_approval_does_not_apply() {
        let g = grant(50_000, 0, Some(10_000), None);
        // Consumed: the in-envelope proposal falls back to the ask.
        let consumed = IntentApproval {
            consumed_at: Some(NOW - 10),
            ..approval()
        };
        assert!(matches!(
            decide(&g, 1_000, 100, Some(&consumed)),
            ApprovalDecision::Deny(DenyReason::AskRequired)
        ));
        // Expired — inclusive boundary, like the grant.
        let expired = IntentApproval {
            expires_at: NOW,
            ..approval()
        };
        assert!(matches!(
            decide(&g, 1_000, 100, Some(&expired)),
            ApprovalDecision::Deny(DenyReason::AskRequired)
        ));
        assert!(expired.is_expired(NOW));
        // Wrong digest.
        let other = IntentApproval {
            intent_digest: "other".into(),
            ..approval()
        };
        assert!(matches!(
            decide(&g, 1_000, 100, Some(&other)),
            ApprovalDecision::Deny(DenyReason::AskRequired)
        ));
    }

    /// The core invariant, swept: no send request — routine, at a cap,
    /// zero, or enormous — reaches an allow without an approval.
    #[test]
    fn a_send_never_allows_without_an_approval() {
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
                assert_ne!(
                    authorize_spend(g, r, NOW),
                    Decision::Allow,
                    "grant-alone allow for {r:?}"
                );
                assert!(
                    matches!(
                        evaluate_send_with_approval(g, RECIPIENT, r, DIGEST, None, NOW),
                        ApprovalDecision::Deny(_)
                    ),
                    "approval-free allow for {r:?}"
                );
            }
        }
    }

    #[test]
    fn approved_reserve_cannot_overspend_the_budget() {
        let mut g = grant(1_000, 0, None, None);
        let mut a = approval();
        // The budget is a grant boundary: the approval does not lift it,
        // and a denied reserve neither draws budget nor spends the
        // approval.
        assert!(matches!(
            g.reserve_send(RECIPIENT, &req(5_000, 100), DIGEST, Some(&mut a), NOW),
            Err(DenyReason::OverBudget { .. })
        ));
        assert_eq!(g.spent_sat, 0, "denied reserve must not draw down");
        assert_eq!(a.consumed_at, None, "denied reserve must not consume");
        // The same approval still authorizes an in-budget proposal with
        // the same digest.
        g.reserve_send(RECIPIENT, &req(800, 100), DIGEST, Some(&mut a), NOW)
            .unwrap();
        assert_eq!(g.spent_sat, 900);
        assert_eq!(g.remaining_sat(), 100);
        assert_eq!(a.consumed_at, Some(NOW));
    }

    #[test]
    fn refund_after_approved_reserve_restores_spent() {
        let mut g = grant(1_000, 0, None, None);
        let mut a = approval();
        let r = req(800, 100);
        g.reserve_send(RECIPIENT, &r, DIGEST, Some(&mut a), NOW)
            .unwrap();
        g.refund(&r);
        assert_eq!(g.spent_sat, 0);
        assert_eq!(g.tx_count, 0);
        // The approval stays consumed: refund restores budget, not the
        // authorization. Re-arming takes a fresh human approval.
        assert_eq!(a.consumed_at, Some(NOW));
    }

    #[test]
    fn wrapped_decision_agrees_with_authorize_intent_when_no_approval() {
        let g = grant(50_000, 45_412, Some(10_000), Some(1_000));
        for (amount, fee) in [(4_500, 88), (4_500, 89), (10_001, 0), (0, 1_001), (0, 0)] {
            let plain = authorize_intent(&g, &IntentRequest::Send(req(amount, fee)), NOW);
            let wrapped = decide(&g, amount, fee, None);
            match plain {
                Decision::Deny(reason) => {
                    assert_eq!(
                        wrapped,
                        ApprovalDecision::Deny(reason),
                        "diverged for amount {amount} fee {fee}"
                    );
                }
                Decision::Allow => panic!("the ladder allowed a spend for amount {amount}"),
            }
        }
    }

    #[test]
    fn approval_json_round_trip() {
        let a = approval();
        let json = serde_json::to_value(&a).unwrap();
        assert!(json.get("consumed_at").is_none(), "None fields are omitted");
        let back: IntentApproval = serde_json::from_value(json).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn observe_mode_denies_sends_and_swaps() {
        let mut g = grant(50_000, 0, None, None);
        g.mode = GrantMode::Observe;
        assert_eq!(
            evaluate_send(&g, RECIPIENT, &req(1, 1), NOW),
            Decision::Deny(DenyReason::ObserveOnly)
        );
        assert!(matches!(
            authorize_intent(&g, &swap(1, 1), NOW),
            Decision::Deny(DenyReason::ObserveOnly),
        ));
    }

    /// The intra-ladder order is deliberate: the specific boundary
    /// answers before the terminal ask, so a refusal names the boundary
    /// that was crossed instead of the generic ask.
    #[test]
    fn caps_speak_before_the_terminal_ask() {
        let g = grant(50_000, 0, Some(10_000), None);
        assert_eq!(
            evaluate_send(&g, RECIPIENT, &req(1_000, 10), NOW),
            Decision::Deny(DenyReason::AskRequired),
            "in-envelope: the routine ask"
        );
        assert!(
            matches!(
                evaluate_send(&g, RECIPIENT, &req(20_000, 10), NOW),
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
            evaluate_send(&g, RECIPIENT, &req(u64::MAX, u64::MAX), NOW),
            Decision::Deny(DenyReason::AmountOverflow { .. })
        ));
        assert!(matches!(
            evaluate_send(&g, RECIPIENT, &req(30_000, 10), NOW),
            Decision::Deny(DenyReason::RecipientNotAllowed { .. })
        ));
    }

    #[test]
    fn recipient_rule_is_a_hard_boundary() {
        let mut g = grant(50_000, 0, None, None);
        g.allowed_recipients = Some(vec![RECIPIENT.to_string(), "tb1pother".into()]);
        assert_eq!(
            evaluate_send(&g, RECIPIENT, &req(1_000, 10), NOW),
            Decision::Deny(DenyReason::AskRequired),
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
            evaluate_send(&g, RECIPIENT, &req(1_000, 10), NOW),
            Decision::Deny(DenyReason::RecipientNotAllowed { .. })
        ));
        // The recipient-blind view skips the rule (it has nothing to
        // judge) and falls through to the terminal ask; only the
        // daemon's evaluate_send enforces the allowlist.
        assert_eq!(
            authorize_spend(&g, &req(1_000, 10), NOW),
            Decision::Deny(DenyReason::AskRequired)
        );
    }

    /// The approvable predicate is total and frozen: every variant has an
    /// explicit expected answer here, so adding a variant forces a
    /// decision. Exactly one refusal is approvable — the terminal ask.
    #[test]
    fn approvable_truth_table() {
        let cases: Vec<(DenyReason, bool)> = vec![
            (DenyReason::Expired { expired_at: 1 }, false),
            (
                DenyReason::OverMaxTx {
                    requested_sat: 2,
                    max_tx_sat: 1,
                },
                false,
            ),
            (
                DenyReason::OverMaxFee {
                    fee_sat: 2,
                    max_fee_sat: 1,
                },
                false,
            ),
            (
                DenyReason::OverBudget {
                    requested_sat: 2,
                    remaining_sat: 1,
                },
                false,
            ),
            (
                DenyReason::IntentNotGranted {
                    intent: "swap".into(),
                },
                false,
            ),
            (
                DenyReason::AmountOverflow {
                    amount_sat: u64::MAX,
                    fee_sat: 1,
                },
                false,
            ),
            (DenyReason::AskRequired, true),
            (DenyReason::ObserveOnly, false),
            (
                DenyReason::RecipientNotAllowed {
                    recipient: "tb1p".into(),
                },
                false,
            ),
        ];
        let approvable = cases
            .iter()
            .filter(|(reason, _)| reason.approvable())
            .count();
        assert_eq!(approvable, 1, "exactly one approvable refusal");
        for (reason, expected) in cases {
            assert_eq!(
                reason.approvable(),
                expected,
                "approvable({}) changed",
                reason.code()
            );
        }
    }

    #[test]
    fn deny_reasons_serialize_with_stable_tags() {
        for (reason, code) in [
            (DenyReason::AskRequired, "ask_required"),
            (DenyReason::ObserveOnly, "observe_only"),
            (
                DenyReason::RecipientNotAllowed {
                    recipient: "tb1p".into(),
                },
                "recipient_not_allowed",
            ),
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
            (DenyReason::Expired { expired_at: 9 }, "expired"),
        ] {
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
        let listed = Grant {
            allowed_recipients: Some(vec!["tb1pother".into()]),
            ..grant(50_000, 0, None, None)
        };
        let cases: Vec<(Grant, SpendRequest, u64, Option<IntentApproval>)> = vec![
            // Unapproved terminal ask.
            (grant(50_000, 0, None, None), req(1_000, 10), NOW, None),
            // Expired, even with a matching approval in hand.
            (expired, req(1_000, 10), 2_000, Some(approval())),
            // Observe-only, approval in hand.
            (observe, req(1_000, 10), NOW, Some(approval())),
            // Over the hard amount cap, approval in hand.
            (capped, req(10_001, 10), NOW, Some(approval())),
            // Recipient outside the allowlist, approval in hand.
            (listed, req(1_000, 10), NOW, Some(approval())),
            // Consumed approval on an identical retry.
            (
                grant(50_000, 0, None, None),
                req(1_000, 10),
                NOW,
                Some(IntentApproval {
                    consumed_at: Some(NOW - 1),
                    ..approval()
                }),
            ),
            // Approval bound to a different digest.
            (
                grant(50_000, 0, None, None),
                req(1_000, 10),
                NOW,
                Some(IntentApproval {
                    intent_digest: "other".into(),
                    ..approval()
                }),
            ),
            // Arithmetic overflow, approval in hand.
            (
                grant(u64::MAX, 0, None, None),
                req(u64::MAX, u64::MAX),
                NOW,
                Some(approval()),
            ),
        ];
        for (mut g, r, now, approval) in cases {
            let spent_before = g.spent_sat;
            let tx_before = g.tx_count;
            let mut holder = approval;
            let result = g.reserve_send(RECIPIENT, &r, DIGEST, holder.as_mut(), now);
            assert!(result.is_err(), "expected a denial for {r:?} at {now}");
            assert_eq!(g.spent_sat, spent_before, "denied send drew budget");
            assert_eq!(g.tx_count, tx_before, "denied send counted a tx");
        }
    }
}
