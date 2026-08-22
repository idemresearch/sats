//! The authorization engine: deterministic enforcement of human-set
//! budgets for agent spending.
//!
//! [`authorize_spend`] is pure — no clock, no IO — and is the single
//! decision point shared by every frontend. Callers reserve budget
//! *before* signing (a signed transaction is already spendable), and
//! refund only when signing fails.

use serde::{Deserialize, Serialize};

use crate::fmt::format_sats;
use crate::seal::SealedBlob;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    pub agent: String,
    /// Network name, so a grant file copied across networks is inert.
    pub network: String,
    pub budget_sat: u64,
    /// Running total of amount + fee for every reserved spend.
    pub spent_sat: u64,
    /// Per-transaction amount cap (excluding fee).
    pub max_tx_sat: Option<u64>,
    /// Per-transaction fee cap.
    pub max_fee_sat: Option<u64>,
    pub created_at: u64,
    pub expires_at: u64,
    pub tx_count: u64,
    /// The master seed re-sealed under `grant_key` — see the V1 trust
    /// model in the README. Deleting the grant file is revocation.
    pub wrapped_seed: SealedBlob,
    /// Base64 32-byte key for `wrapped_seed`.
    pub grant_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpendRequest {
    pub amount_sat: u64,
    pub fee_sat: u64,
}

impl SpendRequest {
    /// What the spend draws from the budget: amount + fee.
    pub fn total_sat(&self) -> u64 {
        self.amount_sat.saturating_add(self.fee_sat)
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

    /// What the intent draws from the budget: sats out + fee.
    pub fn total_sat(&self) -> u64 {
        self.sats_out().saturating_add(self.fee_sat())
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(DenyReason),
}

/// PURE. Send-shaped view of [`authorize_intent`], for callers that only
/// ever send. Both paths make the identical decision.
pub fn authorize_spend(grant: &Grant, req: &SpendRequest, now_unix: u64) -> Decision {
    authorize_intent(grant, &IntentRequest::Send(*req), now_unix)
}

/// PURE. The single deterministic enforcement point for every intent.
/// Check order: expiry → intent authority → per-tx amount cap →
/// per-tx fee cap → budget. Expiry outranks everything, including
/// whether the intent kind is granted at all.
pub fn authorize_intent(grant: &Grant, req: &IntentRequest, now_unix: u64) -> Decision {
    if grant.is_expired(now_unix) {
        return Decision::Deny(DenyReason::Expired {
            expired_at: grant.expires_at,
        });
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
    if req.total_sat() > remaining_sat {
        return Decision::Deny(DenyReason::OverBudget {
            requested_sat: req.total_sat(),
            remaining_sat,
        });
    }
    Decision::Allow
}

impl Grant {
    pub fn remaining_sat(&self) -> u64 {
        self.budget_sat.saturating_sub(self.spent_sat)
    }

    pub fn is_expired(&self, now_unix: u64) -> bool {
        now_unix >= self.expires_at
    }

    /// Re-check and draw the spend down from the budget. Callers must
    /// persist the grant after a successful reserve, before signing.
    pub fn reserve(&mut self, req: &SpendRequest, now_unix: u64) -> Result<(), DenyReason> {
        match authorize_spend(self, req, now_unix) {
            Decision::Allow => {
                self.spent_sat = self.spent_sat.saturating_add(req.total_sat());
                self.tx_count += 1;
                Ok(())
            }
            Decision::Deny(reason) => Err(reason),
        }
    }

    /// Return a reservation whose signing failed. Never refund after a
    /// signature exists — a signed transaction is spendable regardless of
    /// whether the broadcast succeeded.
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal;

    fn grant(budget: u64, spent: u64, max_tx: Option<u64>, max_fee: Option<u64>) -> Grant {
        let key = seal::decode_key_b64(&seal::generate_key_b64().unwrap()).unwrap();
        Grant {
            agent: "test".into(),
            network: "signet".into(),
            budget_sat: budget,
            spent_sat: spent,
            max_tx_sat: max_tx,
            max_fee_sat: max_fee,
            created_at: 1_000,
            expires_at: 2_000,
            tx_count: 0,
            wrapped_seed: seal::seal_with_key(b"seed", &key, b"test").unwrap(),
            grant_key: seal::generate_key_b64().unwrap(),
        }
    }

    const NOW: u64 = 1_500;

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

    #[test]
    fn reserve_draws_down_and_recheck_denies() {
        let mut g = grant(10_000, 0, None, None);
        g.reserve(&req(6_000, 100), NOW).unwrap();
        assert_eq!(g.spent_sat, 6_100);
        assert_eq!(g.tx_count, 1);
        // Second spend that no longer fits.
        let err = g.reserve(&req(4_000, 100), NOW).unwrap_err();
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
        g.reserve(&r, NOW).unwrap();
        g.refund(&r);
        assert_eq!(g.spent_sat, 0);
        assert_eq!(g.tx_count, 0);
        assert_eq!(authorize_spend(&g, &req(9_900, 100), NOW), Decision::Allow);
    }

    #[test]
    fn total_saturates_instead_of_overflowing() {
        let g = grant(u64::MAX, 0, None, None);
        let r = req(u64::MAX, u64::MAX);
        assert_eq!(r.total_sat(), u64::MAX);
        assert_eq!(authorize_spend(&g, &r, NOW), Decision::Allow);
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
}
