//! The authorization engine: deterministic enforcement of human-set
//! authority for agent spending.
//!
//! [`evaluate_send`] is pure — no clock, no IO — and is the single
//! ladder every frontend decides through: a hard envelope (expiry,
//! suspension, observe mode, intent authority, overflow, the hard
//! ceiling) whose refusals are never approvable, then the ask band
//! (recipient rule, ask mode, caps, budget) whose refusals a one-time
//! human approval can lift. Callers reserve budget *before* signing (a
//! signed transaction is already spendable), and refund only when
//! signing fails.

use serde::{Deserialize, Serialize};

use crate::fmt::format_sats;
use crate::token;

/// Current on-disk grant shape.
///
/// Version 1 stored the master seed re-sealed under a key in the same
/// file; it is read for diagnosis and refused for signing. Version 2
/// records deserialize with every v3 field at its default — mode `auto`,
/// no hard ceiling, no recipient rule, not suspended, no strikes —
/// which is exactly their prior behavior. Versions above 3 are refused
/// by `Store::load_grant`: a record written by a newer sats may carry
/// restrictions this build cannot see, and ignoring them would widen
/// authority. See `Store::load_grant`.
pub const GRANT_FORMAT_VERSION: u32 = 3;

const fn grant_format_version() -> u32 {
    GRANT_FORMAT_VERSION
}

/// How much standing autonomy a grant carries. The order is an authority
/// order — `auto > ask > observe` — so tightening is cheap and widening
/// is a control-plane act.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantMode {
    /// Sends inside the caps execute without a human.
    #[default]
    Auto,
    /// Every send needs a one-time human approval; the caps still bound
    /// what may be asked for.
    Ask,
    /// Read-only: no send can be authorized, and no approval can lift it.
    Observe,
}

impl GrantMode {
    pub fn as_str(self) -> &'static str {
        match self {
            GrantMode::Auto => "auto",
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
            GrantMode::Auto => 2,
        };
        rank(next) > rank(self)
    }
}

impl std::str::FromStr for GrantMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(GrantMode::Auto),
            "ask" => Ok(GrantMode::Ask),
            "observe" => Ok(GrantMode::Observe),
            other => Err(format!("unknown mode {other:?} (auto, ask, or observe)")),
        }
    }
}

/// Why a grant's autonomy is suspended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuspendTrigger {
    /// A human suspended it explicitly.
    Manual,
    /// The refusal-storm breaker tripped.
    DenialStorm,
}

impl SuspendTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            SuspendTrigger::Manual => "manual",
            SuspendTrigger::DenialStorm => "denial_storm",
        }
    }
}

/// The STOP state: autonomy is withdrawn until a human resumes the
/// grant. Set by the breaker or by an explicit suspend; only a
/// password-gated resume (or a grant replacement) clears it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suspension {
    pub at: u64,
    pub trigger: SuspendTrigger,
}

/// One noted policy refusal, for the storm breaker: which request, when.
/// Deduplicated by request id, so retrying one request never stacks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Strike {
    pub request_id: String,
    pub at: u64,
}

/// Distinct refused requests within [`STOP_WINDOW_SECS`] that trip the
/// storm breaker.
pub const STOP_AFTER_REFUSALS: usize = 10;
/// The breaker's sliding window, in seconds.
pub const STOP_WINDOW_SECS: u64 = 600;
/// Ceiling on stored strikes; the oldest are dropped past it.
pub const MAX_STRIKES: usize = 32;

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
    /// Per-transaction amount cap (excluding fee): the automatic band.
    /// An amount above it is refused, and the refusal is approvable.
    pub max_tx_sat: Option<u64>,
    /// Per-transaction fee cap.
    pub max_fee_sat: Option<u64>,
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
    /// Standing authority mode. Absent (v2 records) means `auto` —
    /// exactly their prior behavior.
    #[serde(default)]
    pub mode: GrantMode,
    /// Hard per-transaction ceiling: an amount above it is never
    /// approvable — the human pre-committed, and the only escalation is
    /// changing the grant. `None` keeps the v2 behavior where everything
    /// above `max_tx_sat` is a one-time-approvable ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_max_tx_sat: Option<u64>,
    /// Standing recipient allowlist, in the same normalized spelling the
    /// intent digest hashes. `None` means unrestricted; `Some(vec![])`
    /// means no recipient is automatic, so every send asks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_recipients: Option<Vec<String>>,
    /// STOP: while set, every intent is refused `suspended` and no
    /// approval lifts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspended: Option<Suspension>,
    /// Recent refusals for the storm breaker, deduplicated by request id
    /// and pruned to the window. Bounded by [`MAX_STRIKES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub strikes: Vec<Strike>,
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
    ApprovalFeeExceeded {
        fee_sat: u64,
        max_fee_sat: u64,
    },
    /// Amount + fee overflows u64. Arithmetically absurd, so it fails
    /// closed instead of saturating into a number a budget could pass.
    AmountOverflow {
        amount_sat: u64,
        fee_sat: u64,
    },
    /// The grant is in ask mode: every send needs a one-time approval.
    AskRequired,
    /// Above the hard per-transaction ceiling. Never approvable: the
    /// human pre-committed at grant time, and the only escalation is
    /// changing the grant itself.
    OverAskMax {
        requested_sat: u64,
        ask_max_tx_sat: u64,
    },
    /// The grant is observe-only. Never approvable.
    ObserveOnly,
    /// STOP: autonomy is suspended until a human resumes the grant.
    /// Never approvable.
    Suspended {
        suspended_at: u64,
    },
    /// The recipient is outside the grant's standing allowlist. The
    /// refusal is approvable — the approval binds exactly this recipient
    /// by intent digest and never widens the standing list.
    RecipientNotAllowed {
        recipient: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(DenyReason),
}

/// A one-time human exception bound to an exact intent digest.
///
/// An approval can lift only the grant's quantitative caps (per-tx
/// amount, per-tx fee, budget). It never survives revocation (the grant
/// file, and with it the signing key, is gone) and never outranks grant
/// expiry. Consumption is permanent: one approval authorizes at most one
/// signature, ever.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentApproval {
    /// Digest of the exact [`crate::intent::SendIntent`] the human saw.
    pub intent_digest: String,
    pub approved_at: u64,
    /// Inclusive, matching the grant convention: now >= expires_at is
    /// expired.
    pub expires_at: u64,
    /// Fee ceiling for the approved send. The amount is bound exactly by
    /// the digest, so this is equivalently a total ceiling.
    pub max_fee_sat: u64,
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

/// The decision of [`authorize_intent_with_approval`]: an allow records
/// which authority it drew on, so callers consume the approval only when
/// it was actually needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    AllowByGrant,
    AllowByApproval,
    Deny(DenyReason),
}

/// Which authority a successful reservation drew on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveVia {
    Grant,
    Approval,
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
/// The hard envelope runs first, and none of its refusals is approvable:
/// expiry → suspension → observe mode → intent authority → arithmetic
/// overflow → hard amount ceiling. Only then the ask band: recipient
/// rule (when the caller supplies a recipient) → ask mode → per-tx
/// amount cap → per-tx fee cap → budget. An approvable reason can never
/// mask a hard one.
fn ladder(grant: &Grant, recipient: Option<&str>, req: &IntentRequest, now_unix: u64) -> Decision {
    // Expiry outranks everything, including suspension: a dead grant
    // reports dead, because resuming it would change nothing.
    if grant.is_expired(now_unix) {
        return Decision::Deny(DenyReason::Expired {
            expired_at: grant.expires_at,
        });
    }
    if let Some(suspension) = &grant.suspended {
        return Decision::Deny(DenyReason::Suspended {
            suspended_at: suspension.at,
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
    if let Some(ask_max_tx_sat) = grant.ask_max_tx_sat
        && req.amount_sat > ask_max_tx_sat
    {
        return Decision::Deny(DenyReason::OverAskMax {
            requested_sat: req.amount_sat,
            ask_max_tx_sat,
        });
    }
    // The ask band: everything below is refusable-but-approvable.
    if let (Some(recipient), Some(allowed)) = (recipient, &grant.allowed_recipients)
        && !allowed.iter().any(|entry| entry == recipient)
    {
        return Decision::Deny(DenyReason::RecipientNotAllowed {
            recipient: recipient.to_string(),
        });
    }
    if grant.mode == GrantMode::Ask {
        return Decision::Deny(DenyReason::AskRequired);
    }
    if let Some(max_tx_sat) = grant.max_tx_sat
        && req.amount_sat > max_tx_sat
    {
        return Decision::Deny(DenyReason::OverMaxTx {
            requested_sat: req.amount_sat,
            max_tx_sat,
        });
    }
    if let Some(max_fee_sat) = grant.max_fee_sat
        && req.fee_sat > max_fee_sat
    {
        return Decision::Deny(DenyReason::OverMaxFee {
            fee_sat: req.fee_sat,
            max_fee_sat,
        });
    }
    let remaining_sat = grant.remaining_sat();
    if total_sat > remaining_sat {
        return Decision::Deny(DenyReason::OverBudget {
            requested_sat: total_sat,
            remaining_sat,
        });
    }
    Decision::Allow
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
        req.fee_sat,
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
        req.fee_sat(),
        intent_digest,
        approval,
        now_unix,
    )
}

/// The one approval-lifting rule, shared by both decision surfaces.
///
/// The plain decision runs first, unchanged — when it allows, the grant
/// alone carries the spend and the approval is untouched. A valid
/// approval (digest match, unconsumed, unexpired) can lift only a
/// refusal whose [`DenyReason::approvable`] is true, subject to the
/// approval's own fee ceiling. The hard envelope — expiry, ungranted
/// intents, overflow, the hard ceiling, observe mode, suspension — is
/// never liftable: those are the human's pre-commitments and kill
/// switches, and an exception issued earlier must not survive them.
fn lift_with_approval(
    decision: Decision,
    fee_sat: u64,
    intent_digest: &str,
    approval: Option<&IntentApproval>,
    now_unix: u64,
) -> ApprovalDecision {
    let reason = match decision {
        Decision::Allow => return ApprovalDecision::AllowByGrant,
        Decision::Deny(reason) => reason,
    };
    if !reason.approvable() {
        return ApprovalDecision::Deny(reason);
    }
    match approval {
        Some(approval) if approval.is_valid_for(intent_digest, now_unix) => {
            if fee_sat > approval.max_fee_sat {
                ApprovalDecision::Deny(DenyReason::ApprovalFeeExceeded {
                    fee_sat,
                    max_fee_sat: approval.max_fee_sat,
                })
            } else {
                ApprovalDecision::AllowByApproval
            }
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
    /// draw the spend down from the budget. On the approval path the
    /// draw may exceed the budget — `spent_sat` grows past `budget_sat`
    /// and `remaining_sat` saturates to zero — and the approval is
    /// marked consumed. Callers must persist the approval's holder
    /// before the grant, and the grant before signing.
    pub fn reserve_send(
        &mut self,
        recipient: &str,
        req: &SpendRequest,
        intent_digest: &str,
        approval: Option<&mut IntentApproval>,
        now_unix: u64,
    ) -> Result<ReserveVia, DenyReason> {
        let decision = evaluate_send_with_approval(
            self,
            recipient,
            req,
            intent_digest,
            approval.as_deref(),
            now_unix,
        );
        let via = match decision {
            ApprovalDecision::AllowByGrant => ReserveVia::Grant,
            ApprovalDecision::AllowByApproval => {
                if let Some(approval) = approval {
                    approval.consumed_at = Some(now_unix);
                }
                ReserveVia::Approval
            }
            ApprovalDecision::Deny(reason) => return Err(reason),
        };
        self.spent_sat = self.spent_sat.saturating_add(req.total_sat());
        self.tx_count = self.tx_count.saturating_add(1);
        Ok(via)
    }

    /// PURE. Note one durable policy refusal for the storm breaker:
    /// prune strikes older than the window, deduplicate by request id
    /// (retrying one request refreshes its strike, never adds one),
    /// bound storage, and report whether this note reached the
    /// threshold. Persisting the grant — and acting on a trip — is the
    /// caller's job; this only does the arithmetic.
    pub fn note_refusal(&mut self, request_id: &str, now_unix: u64) -> bool {
        self.strikes
            .retain(|strike| now_unix.saturating_sub(strike.at) < STOP_WINDOW_SECS);
        match self
            .strikes
            .iter_mut()
            .find(|strike| strike.request_id == request_id)
        {
            Some(strike) => strike.at = now_unix,
            None => self.strikes.push(Strike {
                request_id: request_id.to_string(),
                at: now_unix,
            }),
        }
        if self.strikes.len() > MAX_STRIKES {
            let excess = self.strikes.len() - MAX_STRIKES;
            self.strikes.drain(..excess);
        }
        self.strikes.len() >= STOP_AFTER_REFUSALS
    }

    /// Forget every strike: a human touched the system — an approval, a
    /// resume, or a grant replacement — which is the breaker's reset.
    pub fn clear_strikes(&mut self) {
        self.strikes.clear();
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
            DenyReason::ApprovalFeeExceeded { .. } => "approval_fee_exceeded",
            DenyReason::AmountOverflow { .. } => "amount_overflow",
            DenyReason::AskRequired => "ask_required",
            DenyReason::OverAskMax { .. } => "over_ask_max",
            DenyReason::ObserveOnly => "observe_only",
            DenyReason::Suspended { .. } => "suspended",
            DenyReason::RecipientNotAllowed { .. } => "recipient_not_allowed",
        }
    }

    /// Whether a one-time human approval can lift this refusal. The hard
    /// envelope — expiry, revocation-adjacent authority, arithmetic
    /// nonsense, the hard ceiling, observe mode, suspension — is never
    /// approvable; the ask band is. Every surface that offers, creates,
    /// or honors an approval must key off this one predicate.
    pub fn approvable(&self) -> bool {
        match self {
            DenyReason::OverMaxTx { .. }
            | DenyReason::OverMaxFee { .. }
            | DenyReason::OverBudget { .. }
            | DenyReason::ApprovalFeeExceeded { .. }
            | DenyReason::AskRequired
            | DenyReason::RecipientNotAllowed { .. } => true,
            DenyReason::Expired { .. }
            | DenyReason::IntentNotGranted { .. }
            | DenyReason::AmountOverflow { .. }
            | DenyReason::OverAskMax { .. }
            | DenyReason::ObserveOnly
            | DenyReason::Suspended { .. } => false,
        }
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
            DenyReason::ApprovalFeeExceeded {
                fee_sat,
                max_fee_sat,
            } => format!(
                "fee           {} sat\napproved max  {} sat",
                format_sats(*fee_sat),
                format_sats(*max_fee_sat)
            ),
            DenyReason::AmountOverflow {
                amount_sat,
                fee_sat,
            } => format!(
                "amount  {} sat\nfee     {} sat\namount + fee overflows",
                format_sats(*amount_sat),
                format_sats(*fee_sat)
            ),
            DenyReason::AskRequired => {
                "grant is in ask mode — every send needs a one-time approval".to_string()
            }
            DenyReason::OverAskMax {
                requested_sat,
                ask_max_tx_sat,
            } => format!(
                "requested  {} sat\nhard max   {} sat — above the approvable ceiling",
                format_sats(*requested_sat),
                format_sats(*ask_max_tx_sat)
            ),
            DenyReason::ObserveOnly => "grant is observe-only".to_string(),
            DenyReason::Suspended { suspended_at: _ } => {
                "grant is suspended — a human can restore it with: sats agent resume".to_string()
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
            max_fee_sat: max_fee,
            created_at: 1_000,
            expires_at: 2_000,
            tx_count: 0,
            token_id: token.token_id,
            token_hash: token.token_hash,
            mode: GrantMode::Auto,
            ask_max_tx_sat: None,
            allowed_recipients: None,
            suspended: None,
            strikes: Vec::new(),
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

    #[test]
    fn allows_within_all_limits() {
        let g = grant(50_000, 0, Some(10_000), Some(1_000));
        assert_eq!(authorize_spend(&g, &req(4_500, 281), NOW), Decision::Allow);
    }

    #[test]
    fn denies_expired_even_when_within_budget() {
        let g = grant(50_000, 0, None, None);
        // Expiry is inclusive: now == expires_at is expired.
        assert_eq!(
            authorize_spend(&g, &req(1, 1), 2_000),
            Decision::Deny(DenyReason::Expired { expired_at: 2_000 })
        );
        assert_eq!(authorize_spend(&g, &req(1, 1), 1_999), Decision::Allow);
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
        // amount at the cap: allowed, even though amount+fee exceeds it.
        assert_eq!(authorize_spend(&g, &req(10_000, 500), NOW), Decision::Allow);
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
            Decision::Allow
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
        // Exactly the budget: allowed.
        assert_eq!(authorize_spend(&g, &req(9_500, 500), NOW), Decision::Allow);
        // One sat over via the fee: denied.
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
        assert_eq!(authorize_spend(&g, &req(4_500, 88), NOW), Decision::Allow);
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
    fn zero_total_request_is_allowed_within_budget() {
        let g = grant(1_000, 1_000, None, None);
        // Remaining is 0; a zero-sat request still fits (0 > 0 is false).
        assert_eq!(authorize_spend(&g, &req(0, 0), NOW), Decision::Allow);
    }

    /// Reserve with no approval in play: the grant alone decides.
    fn reserve(g: &mut Grant, r: &SpendRequest, now: u64) -> Result<ReserveVia, DenyReason> {
        g.reserve_send(RECIPIENT, r, DIGEST, None, now)
    }

    #[test]
    fn reserve_draws_down_and_recheck_denies() {
        let mut g = grant(10_000, 0, None, None);
        reserve(&mut g, &req(6_000, 100), NOW).unwrap();
        assert_eq!(g.spent_sat, 6_100);
        assert_eq!(g.tx_count, 1);
        // Second spend that no longer fits.
        let err = reserve(&mut g, &req(4_000, 100), NOW).unwrap_err();
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
        assert_eq!(authorize_spend(&g, &req(9_900, 100), NOW), Decision::Allow);
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
        // The largest representable total still authorizes.
        assert_eq!(authorize_spend(&g, &req(u64::MAX, 0), NOW), Decision::Allow);
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
        let a = approval(u64::MAX);
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
    }

    const DIGEST: &str = "d1";

    fn approval(max_fee: u64) -> IntentApproval {
        IntentApproval {
            intent_digest: DIGEST.into(),
            approved_at: NOW - 100,
            expires_at: 1_900,
            max_fee_sat: max_fee,
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
    fn approval_overrides_each_quantitative_cap() {
        let a = approval(10_000);
        // Over max-tx.
        let g = grant(50_000, 0, Some(10_000), None);
        assert_eq!(
            decide(&g, 20_000, 100, Some(&a)),
            ApprovalDecision::AllowByApproval
        );
        // Over max-fee.
        let g = grant(50_000, 0, None, Some(100));
        assert_eq!(
            decide(&g, 1_000, 500, Some(&a)),
            ApprovalDecision::AllowByApproval
        );
        // Over budget.
        let g = grant(1_000, 0, None, None);
        assert_eq!(
            decide(&g, 5_000, 100, Some(&a)),
            ApprovalDecision::AllowByApproval
        );
    }

    #[test]
    fn approval_never_overrides_expiry() {
        let g = grant(50_000, 0, Some(10), None);
        let a = IntentApproval {
            expires_at: 10_000,
            ..approval(10_000)
        };
        assert!(matches!(
            authorize_intent_with_approval(
                &g,
                &IntentRequest::Send(req(20_000, 100)),
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
        let a = approval(10_000);
        assert!(matches!(
            authorize_intent_with_approval(&g, &swap(1_000, 100), DIGEST, Some(&a), NOW),
            ApprovalDecision::Deny(DenyReason::IntentNotGranted { .. })
        ));
    }

    #[test]
    fn spent_or_stale_approval_does_not_apply() {
        let g = grant(50_000, 0, Some(10_000), None);
        // Consumed.
        let consumed = IntentApproval {
            consumed_at: Some(NOW - 10),
            ..approval(10_000)
        };
        assert!(matches!(
            decide(&g, 20_000, 100, Some(&consumed)),
            ApprovalDecision::Deny(DenyReason::OverMaxTx { .. })
        ));
        // Expired — inclusive boundary, like the grant.
        let expired = IntentApproval {
            expires_at: NOW,
            ..approval(10_000)
        };
        assert!(matches!(
            decide(&g, 20_000, 100, Some(&expired)),
            ApprovalDecision::Deny(DenyReason::OverMaxTx { .. })
        ));
        assert!(expired.is_expired(NOW));
        // Wrong digest.
        let other = IntentApproval {
            intent_digest: "other".into(),
            ..approval(10_000)
        };
        assert!(matches!(
            decide(&g, 20_000, 100, Some(&other)),
            ApprovalDecision::Deny(DenyReason::OverMaxTx { .. })
        ));
    }

    #[test]
    fn approval_fee_ceiling_denies_with_typed_code() {
        let g = grant(50_000, 0, Some(10_000), None);
        let a = approval(50);
        let ApprovalDecision::Deny(reason) = decide(&g, 20_000, 51, Some(&a)) else {
            panic!("expected a denial");
        };
        assert_eq!(
            reason,
            DenyReason::ApprovalFeeExceeded {
                fee_sat: 51,
                max_fee_sat: 50
            }
        );
        assert_eq!(reason.code(), "approval_fee_exceeded");
        let json = serde_json::to_value(&reason).unwrap();
        assert_eq!(json["reason"], "approval_fee_exceeded");
        assert_eq!(json["fee_sat"], 51);
        assert_eq!(json["max_fee_sat"], 50);
        // At the ceiling: allowed, matching every other inclusive cap.
        assert_eq!(
            decide(&g, 20_000, 50, Some(&a)),
            ApprovalDecision::AllowByApproval
        );
    }

    #[test]
    fn grant_allow_leaves_approval_unconsumed() {
        let mut g = grant(50_000, 0, None, None);
        let mut a = approval(10_000);
        assert_eq!(
            decide(&g, 1_000, 100, Some(&a)),
            ApprovalDecision::AllowByGrant
        );
        let via = g
            .reserve_send(RECIPIENT, &req(1_000, 100), DIGEST, Some(&mut a), NOW)
            .unwrap();
        assert_eq!(via, ReserveVia::Grant);
        assert!(a.consumed_at.is_none(), "grant path must not consume");
        assert_eq!(g.spent_sat, 1_100);
        assert_eq!(g.tx_count, 1);
    }

    #[test]
    fn approval_reserve_consumes_and_may_overspend() {
        let mut g = grant(1_000, 0, None, None);
        let mut a = approval(10_000);
        let via = g
            .reserve_send(RECIPIENT, &req(5_000, 100), DIGEST, Some(&mut a), NOW)
            .unwrap();
        assert_eq!(via, ReserveVia::Approval);
        assert_eq!(a.consumed_at, Some(NOW));
        assert_eq!(g.spent_sat, 5_100, "draw exceeds the budget");
        assert_eq!(g.remaining_sat(), 0, "remaining saturates");
        // The consumed approval is spent: an identical retry denies.
        assert!(matches!(
            g.reserve_send(RECIPIENT, &req(5_000, 100), DIGEST, Some(&mut a), NOW),
            Err(DenyReason::OverBudget { .. })
        ));
        // And an ordinary in-budget send now denies over_budget too.
        assert!(matches!(
            reserve(&mut g, &req(100, 1), NOW),
            Err(DenyReason::OverBudget { .. })
        ));
    }

    #[test]
    fn refund_after_approval_reserve_restores_spent() {
        let mut g = grant(1_000, 0, None, None);
        let mut a = approval(10_000);
        let r = req(5_000, 100);
        g.reserve_send(RECIPIENT, &r, DIGEST, Some(&mut a), NOW)
            .unwrap();
        g.refund(&r);
        assert_eq!(g.spent_sat, 0);
        assert_eq!(g.tx_count, 0);
        // The approval stays consumed: refund restores budget, not the
        // exception. Re-arming takes a fresh human approval.
        assert_eq!(a.consumed_at, Some(NOW));
    }

    #[test]
    fn wrapped_decision_agrees_with_authorize_intent_when_no_approval() {
        let g = grant(50_000, 45_412, Some(10_000), Some(1_000));
        for (amount, fee) in [(4_500, 88), (4_500, 89), (10_001, 0), (0, 1_001), (0, 0)] {
            let plain = authorize_intent(&g, &IntentRequest::Send(req(amount, fee)), NOW);
            let wrapped = decide(&g, amount, fee, None);
            match (plain, wrapped) {
                (Decision::Allow, ApprovalDecision::AllowByGrant) => {}
                (Decision::Deny(a), ApprovalDecision::Deny(b)) if a == b => {}
                (plain, wrapped) => {
                    panic!("diverged for amount {amount} fee {fee}: {plain:?} vs {wrapped:?}")
                }
            }
        }
    }

    #[test]
    fn approval_json_round_trip() {
        let a = approval(10_000);
        let json = serde_json::to_value(&a).unwrap();
        assert!(json.get("consumed_at").is_none(), "None fields are omitted");
        let back: IntentApproval = serde_json::from_value(json).unwrap();
        assert_eq!(back, a);
    }

    // ---- grant v3: the extended ladder ----------------------------------

    /// A frozen v2 record, exactly as a pre-v3 sats wrote it. It must
    /// deserialize to auto / unrestricted / unsuspended and decide
    /// exactly as it always has.
    #[test]
    fn v2_grant_fixture_reads_with_prior_semantics() {
        let fixture = r#"{
            "format_version": 2,
            "agent": "claude",
            "network": "signet",
            "budget_sat": 50000,
            "spent_sat": 4781,
            "max_tx_sat": 10000,
            "max_fee_sat": 1000,
            "created_at": 1000,
            "expires_at": 2000,
            "tx_count": 1,
            "token_id": "aaaaaaaaaaaa",
            "token_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }"#;
        let g: Grant = serde_json::from_str(fixture).unwrap();
        assert_eq!(g.format_version, 2);
        assert_eq!(g.mode, GrantMode::Auto);
        assert_eq!(g.ask_max_tx_sat, None);
        assert_eq!(g.allowed_recipients, None);
        assert_eq!(g.suspended, None);
        assert!(g.strikes.is_empty());
        // The exact decisions a v2 sats made.
        assert_eq!(authorize_spend(&g, &req(4_500, 281), NOW), Decision::Allow);
        assert_eq!(
            evaluate_send(&g, RECIPIENT, &req(4_500, 281), NOW),
            Decision::Allow,
            "no recipient rule without an allowlist"
        );
        assert!(matches!(
            authorize_spend(&g, &req(10_001, 0), NOW),
            Decision::Deny(DenyReason::OverMaxTx { .. })
        ));
        assert!(matches!(
            authorize_spend(&g, &req(1, 1), 2_000),
            Decision::Deny(DenyReason::Expired { .. })
        ));
    }

    /// A v3 record whose new fields are all defaulted decides identically
    /// to the v2 fixture above.
    #[test]
    fn defaulted_v3_decides_like_v2() {
        let g = grant(50_000, 4_781, Some(10_000), Some(1_000));
        for (amount, fee) in [(4_500, 281), (10_001, 0), (0, 1_001), (45_000, 300), (0, 0)] {
            assert_eq!(
                authorize_spend(&g, &req(amount, fee), NOW),
                evaluate_send(&g, RECIPIENT, &req(amount, fee), NOW),
                "diverged for amount {amount} fee {fee}"
            );
        }
    }

    fn suspended_grant(trigger: SuspendTrigger) -> Grant {
        Grant {
            suspended: Some(Suspension {
                at: NOW - 10,
                trigger,
            }),
            ..grant(50_000, 0, None, None)
        }
    }

    #[test]
    fn expiry_still_outranks_suspension() {
        let g = suspended_grant(SuspendTrigger::DenialStorm);
        assert!(matches!(
            evaluate_send(&g, RECIPIENT, &req(1, 1), 5_000),
            Decision::Deny(DenyReason::Expired { .. })
        ));
        // Unexpired: the suspension speaks.
        assert_eq!(
            evaluate_send(&g, RECIPIENT, &req(1, 1), NOW),
            Decision::Deny(DenyReason::Suspended {
                suspended_at: NOW - 10
            })
        );
    }

    #[test]
    fn suspension_masks_every_ask_band_reason_and_allow() {
        let mut g = suspended_grant(SuspendTrigger::Manual);
        g.mode = GrantMode::Ask;
        g.allowed_recipients = Some(vec![]);
        g.max_tx_sat = Some(10);
        // Every one of these would otherwise be a different refusal (or
        // an allow); suspended answers first.
        for r in [req(1, 1), req(1_000, 0), req(u64::MAX, u64::MAX)] {
            assert!(matches!(
                evaluate_send(&g, RECIPIENT, &r, NOW),
                Decision::Deny(DenyReason::Suspended { .. })
            ));
        }
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

    #[test]
    fn ask_mode_asks_for_an_otherwise_allowed_send() {
        let mut g = grant(50_000, 0, Some(10_000), None);
        g.mode = GrantMode::Ask;
        assert_eq!(
            evaluate_send(&g, RECIPIENT, &req(1_000, 10), NOW),
            Decision::Deny(DenyReason::AskRequired)
        );
        // Ask mode empties the auto band: max_tx is subsumed (everything
        // asks anyway) and the answer stays ask_required. Only the hard
        // ceiling still bounds what may be asked for.
        assert_eq!(
            evaluate_send(&g, RECIPIENT, &req(20_000, 10), NOW),
            Decision::Deny(DenyReason::AskRequired)
        );
        g.ask_max_tx_sat = Some(25_000);
        assert!(matches!(
            evaluate_send(&g, RECIPIENT, &req(30_000, 10), NOW),
            Decision::Deny(DenyReason::OverAskMax { .. })
        ));
    }

    #[test]
    fn hard_ceiling_bands_the_amount() {
        let mut g = grant(1_000_000, 0, Some(10_000), None);
        g.ask_max_tx_sat = Some(25_000);
        // Auto band.
        assert_eq!(
            evaluate_send(&g, RECIPIENT, &req(10_000, 10), NOW),
            Decision::Allow
        );
        // Ask band: inclusive at the ceiling, like every other cap.
        assert!(matches!(
            evaluate_send(&g, RECIPIENT, &req(25_000, 10), NOW),
            Decision::Deny(DenyReason::OverMaxTx { .. })
        ));
        // Above the ceiling: the hard refusal.
        assert_eq!(
            evaluate_send(&g, RECIPIENT, &req(25_001, 10), NOW),
            Decision::Deny(DenyReason::OverAskMax {
                requested_sat: 25_001,
                ask_max_tx_sat: 25_000
            })
        );
    }

    /// An approvable reason must never mask a hard one: the hard ceiling
    /// and overflow answer before the recipient rule and ask mode.
    #[test]
    fn hard_envelope_outranks_the_ask_band() {
        let mut g = grant(u64::MAX, 0, Some(10_000), None);
        g.ask_max_tx_sat = Some(25_000);
        g.mode = GrantMode::Ask;
        g.allowed_recipients = Some(vec!["tb1pother".into()]);
        // Recipient not listed AND ask mode AND over the hard ceiling:
        // the hard ceiling speaks.
        assert!(matches!(
            evaluate_send(&g, RECIPIENT, &req(30_000, 10), NOW),
            Decision::Deny(DenyReason::OverAskMax { .. })
        ));
        // Overflow outranks everything below the mode checks.
        assert!(matches!(
            evaluate_send(&g, RECIPIENT, &req(u64::MAX, u64::MAX), NOW),
            Decision::Deny(DenyReason::AmountOverflow { .. })
        ));
    }

    #[test]
    fn recipient_rule_gates_the_ask_band() {
        let mut g = grant(50_000, 0, None, None);
        g.allowed_recipients = Some(vec![RECIPIENT.to_string(), "tb1pother".into()]);
        assert_eq!(
            evaluate_send(&g, RECIPIENT, &req(1_000, 10), NOW),
            Decision::Allow
        );
        assert_eq!(
            evaluate_send(&g, "tb1pstranger", &req(1_000, 10), NOW),
            Decision::Deny(DenyReason::RecipientNotAllowed {
                recipient: "tb1pstranger".into()
            })
        );
        // An empty allowlist means no recipient is automatic.
        g.allowed_recipients = Some(vec![]);
        assert!(matches!(
            evaluate_send(&g, RECIPIENT, &req(1_000, 10), NOW),
            Decision::Deny(DenyReason::RecipientNotAllowed { .. })
        ));
        // The recipient-blind view skips the rule (it has nothing to
        // judge); only the daemon's evaluate_send enforces it.
        assert_eq!(authorize_spend(&g, &req(1_000, 10), NOW), Decision::Allow);
    }

    #[test]
    fn approval_lifts_ask_band_but_never_hard_envelope() {
        let a = approval(10_000);
        // Ask mode: liftable.
        let mut g = grant(50_000, 0, None, None);
        g.mode = GrantMode::Ask;
        assert_eq!(
            evaluate_send_with_approval(&g, RECIPIENT, &req(1_000, 100), DIGEST, Some(&a), NOW),
            ApprovalDecision::AllowByApproval
        );
        // Recipient rule: liftable.
        let mut g = grant(50_000, 0, None, None);
        g.allowed_recipients = Some(vec!["tb1pother".into()]);
        assert_eq!(
            evaluate_send_with_approval(&g, RECIPIENT, &req(1_000, 100), DIGEST, Some(&a), NOW),
            ApprovalDecision::AllowByApproval
        );
        // Hard ceiling: never.
        let mut g = grant(u64::MAX, 0, None, None);
        g.ask_max_tx_sat = Some(25_000);
        assert!(matches!(
            evaluate_send_with_approval(&g, RECIPIENT, &req(30_000, 100), DIGEST, Some(&a), NOW),
            ApprovalDecision::Deny(DenyReason::OverAskMax { .. })
        ));
        // Observe: never.
        let mut g = grant(50_000, 0, None, None);
        g.mode = GrantMode::Observe;
        assert!(matches!(
            evaluate_send_with_approval(&g, RECIPIENT, &req(1_000, 100), DIGEST, Some(&a), NOW),
            ApprovalDecision::Deny(DenyReason::ObserveOnly)
        ));
        // Suspension: never.
        let g = suspended_grant(SuspendTrigger::DenialStorm);
        assert!(matches!(
            evaluate_send_with_approval(&g, RECIPIENT, &req(1_000, 100), DIGEST, Some(&a), NOW),
            ApprovalDecision::Deny(DenyReason::Suspended { .. })
        ));
    }

    /// The approvable predicate is total and frozen: every variant has an
    /// explicit expected answer here, so adding a variant forces a
    /// decision.
    #[test]
    fn approvable_truth_table() {
        let cases: Vec<(DenyReason, bool)> = vec![
            (DenyReason::Expired { expired_at: 1 }, false),
            (
                DenyReason::OverMaxTx {
                    requested_sat: 2,
                    max_tx_sat: 1,
                },
                true,
            ),
            (
                DenyReason::OverMaxFee {
                    fee_sat: 2,
                    max_fee_sat: 1,
                },
                true,
            ),
            (
                DenyReason::OverBudget {
                    requested_sat: 2,
                    remaining_sat: 1,
                },
                true,
            ),
            (
                DenyReason::IntentNotGranted {
                    intent: "swap".into(),
                },
                false,
            ),
            (
                DenyReason::ApprovalFeeExceeded {
                    fee_sat: 2,
                    max_fee_sat: 1,
                },
                true,
            ),
            (
                DenyReason::AmountOverflow {
                    amount_sat: u64::MAX,
                    fee_sat: 1,
                },
                false,
            ),
            (DenyReason::AskRequired, true),
            (
                DenyReason::OverAskMax {
                    requested_sat: 2,
                    ask_max_tx_sat: 1,
                },
                false,
            ),
            (DenyReason::ObserveOnly, false),
            (DenyReason::Suspended { suspended_at: 1 }, false),
            (
                DenyReason::RecipientNotAllowed {
                    recipient: "tb1p".into(),
                },
                true,
            ),
        ];
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
    fn new_reasons_serialize_with_stable_tags() {
        for (reason, code) in [
            (DenyReason::AskRequired, "ask_required"),
            (
                DenyReason::OverAskMax {
                    requested_sat: 2,
                    ask_max_tx_sat: 1,
                },
                "over_ask_max",
            ),
            (DenyReason::ObserveOnly, "observe_only"),
            (DenyReason::Suspended { suspended_at: 9 }, "suspended"),
            (
                DenyReason::RecipientNotAllowed {
                    recipient: "tb1p".into(),
                },
                "recipient_not_allowed",
            ),
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
        for (mode, s) in [
            (GrantMode::Auto, "auto"),
            (GrantMode::Ask, "ask"),
            (GrantMode::Observe, "observe"),
        ] {
            assert_eq!(serde_json::to_value(mode).unwrap(), s);
            assert_eq!(mode.as_str(), s);
            assert_eq!(s.parse::<GrantMode>().unwrap(), mode);
        }
        assert!("AUTO".parse::<GrantMode>().is_err());
        // Widening needs the password; tightening and staying put do not.
        assert!(GrantMode::Observe.widens_to(GrantMode::Ask));
        assert!(GrantMode::Observe.widens_to(GrantMode::Auto));
        assert!(GrantMode::Ask.widens_to(GrantMode::Auto));
        assert!(!GrantMode::Auto.widens_to(GrantMode::Ask));
        assert!(!GrantMode::Auto.widens_to(GrantMode::Observe));
        assert!(!GrantMode::Ask.widens_to(GrantMode::Observe));
        assert!(!GrantMode::Ask.widens_to(GrantMode::Ask));
    }

    #[test]
    fn v3_grant_round_trips_with_every_field() {
        let mut g = grant(50_000, 0, Some(10_000), Some(1_000));
        g.mode = GrantMode::Ask;
        g.ask_max_tx_sat = Some(25_000);
        g.allowed_recipients = Some(vec![RECIPIENT.to_string()]);
        g.suspended = Some(Suspension {
            at: NOW,
            trigger: SuspendTrigger::DenialStorm,
        });
        g.note_refusal("k-1", NOW);
        let json = serde_json::to_value(&g).unwrap();
        assert_eq!(json["format_version"], 3);
        assert_eq!(json["mode"], "ask");
        assert_eq!(json["suspended"]["trigger"], "denial_storm");
        let back: Grant = serde_json::from_value(json).unwrap();
        assert_eq!(back.mode, GrantMode::Ask);
        assert_eq!(back.ask_max_tx_sat, Some(25_000));
        assert_eq!(back.suspended, g.suspended);
        assert_eq!(back.strikes, g.strikes);

        // Defaulted fields stay off the wire, so quiet grants stay small.
        let quiet = serde_json::to_value(grant(1, 0, None, None)).unwrap();
        assert!(quiet.get("ask_max_tx_sat").is_none());
        assert!(quiet.get("allowed_recipients").is_none());
        assert!(quiet.get("suspended").is_none());
        assert!(quiet.get("strikes").is_none());
        assert_eq!(quiet["mode"], "auto");
    }

    // ---- the storm breaker's pure bookkeeping ---------------------------

    #[test]
    fn note_refusal_counts_distinct_requests_only() {
        let mut g = grant(50_000, 0, None, None);
        for i in 0..STOP_AFTER_REFUSALS - 1 {
            assert!(!g.note_refusal(&format!("k-{i}"), NOW), "strike {i}");
        }
        assert_eq!(g.strikes.len(), STOP_AFTER_REFUSALS - 1);
        // Re-noting the same requests adds nothing.
        for i in 0..STOP_AFTER_REFUSALS - 1 {
            assert!(!g.note_refusal(&format!("k-{i}"), NOW + 1));
        }
        assert_eq!(g.strikes.len(), STOP_AFTER_REFUSALS - 1, "deduplicated");
        // The tenth distinct request trips.
        assert!(g.note_refusal("k-final", NOW + 2));
    }

    #[test]
    fn note_refusal_prunes_the_window() {
        let mut g = grant(50_000, 0, None, None);
        for i in 0..STOP_AFTER_REFUSALS - 1 {
            g.note_refusal(&format!("k-{i}"), NOW);
        }
        // Just inside the window the tenth trips; just past it the old
        // strikes are gone and it does not.
        let mut fresh = g.clone();
        assert!(fresh.note_refusal("k-late", NOW + STOP_WINDOW_SECS - 1));
        assert!(!g.note_refusal("k-late", NOW + STOP_WINDOW_SECS));
        assert_eq!(g.strikes.len(), 1, "expired strikes pruned");
    }

    #[test]
    fn strikes_are_bounded_and_clearable() {
        let mut g = grant(50_000, 0, None, None);
        for i in 0..MAX_STRIKES + 10 {
            g.note_refusal(&format!("k-{i}"), NOW);
        }
        assert_eq!(g.strikes.len(), MAX_STRIKES, "storage is bounded");
        g.clear_strikes();
        assert!(g.strikes.is_empty());
        assert!(!g.note_refusal("k-again", NOW), "cleared means restarted");
    }
}
