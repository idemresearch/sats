//! Transaction preparation and durable finalized-transaction records.
//!
//! A [`PreparedSpend`] exists only while a caller is reviewing, authorizing,
//! and signing a transaction. Normal sends never serialize its PSBT; an
//! explicit export writes a plain PSBT artifact instead. A [`PsbtSession`]
//! is backward-reading state from older releases' staged workflow, while
//! the durable retry/audit record is always a raw finalized transaction.

use std::str::FromStr;

use bdk_wallet::bitcoin::{Psbt, Transaction, consensus};
use serde::{Deserialize, Serialize};

use crate::error::PlanError;

const FORMAT_VERSION: u32 = 1;

const fn format_version() -> u32 {
    FORMAT_VERSION
}

/// A fully constructed spend awaiting review, authorization, and signing.
///
/// This type intentionally does not implement serialization. Callers should
/// keep it in memory unless the user explicitly requested a PSBT export.
#[derive(Debug, Clone)]
pub struct PreparedSpend {
    pub id: String,
    pub network: String,
    pub recipient: String,
    pub amount_sat: u64,
    pub fee_sat: u64,
    pub created_at: u64,
    pub excluded_utxos: u64,
    psbt: Psbt,
}

impl PreparedSpend {
    pub fn new(
        network: String,
        recipient: String,
        amount_sat: u64,
        fee_sat: u64,
        created_at: u64,
        excluded_utxos: u64,
        psbt: Psbt,
    ) -> Self {
        let id = psbt.unsigned_tx.compute_txid().to_string()[..8].to_string();
        PreparedSpend {
            id,
            network,
            recipient,
            amount_sat,
            fee_sat,
            created_at,
            excluded_utxos,
            psbt,
        }
    }

    pub fn total_sat(&self) -> u64 {
        self.amount_sat.saturating_add(self.fee_sat)
    }

    pub fn psbt(&self) -> &Psbt {
        &self.psbt
    }

    /// Materialize the explicit, resumable PSBT workflow.
    pub fn session(&self) -> PsbtSession {
        PsbtSession {
            format_version: FORMAT_VERSION,
            id: self.id.clone(),
            network: self.network.clone(),
            recipient: self.recipient.clone(),
            amount_sat: self.amount_sat,
            fee_sat: self.fee_sat,
            created_at: self.created_at,
            psbt: self.psbt.to_string(),
            excluded_utxos: self.excluded_utxos,
        }
    }

    /// Consume a finalized PSBT and produce the only durable send record.
    pub fn into_transaction(
        self,
        signed_psbt: Psbt,
        source_id: Option<String>,
    ) -> Result<TransactionRecord, PlanError> {
        if signed_psbt.unsigned_tx != self.psbt.unsigned_tx {
            return Err(PlanError::Psbt(
                "signed PSBT does not match the prepared transaction".into(),
            ));
        }
        let tx = signed_psbt
            .extract_tx()
            .map_err(|e| PlanError::Extract(e.to_string()))?;
        Ok(TransactionRecord::from_transaction(
            self.network,
            self.recipient,
            self.amount_sat,
            self.fee_sat,
            self.created_at,
            self.excluded_utxos,
            source_id,
            &tx,
        ))
    }
}

/// An unsigned PSBT persisted by older releases' explicit staged workflow.
/// Retained for backward-reading; new code writes PSBT file artifacts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PsbtSession {
    #[serde(default = "format_version")]
    pub format_version: u32,
    pub id: String,
    pub network: String,
    pub recipient: String,
    pub amount_sat: u64,
    pub fee_sat: u64,
    pub created_at: u64,
    pub psbt: String,
    #[serde(default)]
    pub excluded_utxos: u64,
}

impl PsbtSession {
    pub fn total_sat(&self) -> u64 {
        // Saturating: sessions are deserialized state, so the fields are
        // not trusted to stay within range (matches authz arithmetic).
        self.amount_sat.saturating_add(self.fee_sat)
    }

    pub fn into_prepared(self) -> Result<PreparedSpend, PlanError> {
        if self.format_version != FORMAT_VERSION {
            return Err(PlanError::Psbt(format!(
                "unsupported PSBT session version {}",
                self.format_version
            )));
        }
        let psbt = Psbt::from_str(&self.psbt).map_err(|e| PlanError::Psbt(e.to_string()))?;
        let prepared = PreparedSpend::new(
            self.network,
            self.recipient,
            self.amount_sat,
            self.fee_sat,
            self.created_at,
            self.excluded_utxos,
            psbt,
        );
        if prepared.id != self.id {
            return Err(PlanError::Psbt(
                "PSBT session id does not match its transaction".into(),
            ));
        }
        Ok(prepared)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionStatus {
    Pending,
    Broadcast,
}

/// Durable state after signing. Raw transaction hex is sufficient for retry
/// and avoids retaining PSBT derivation and wallet metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionRecord {
    #[serde(default = "format_version")]
    pub format_version: u32,
    pub txid: String,
    pub network: String,
    pub recipient: String,
    pub amount_sat: u64,
    pub fee_sat: u64,
    pub created_at: u64,
    pub status: TransactionStatus,
    pub tx_hex: String,
    #[serde(default)]
    pub excluded_utxos: u64,
    /// Explicit PSBT-session or legacy-plan id, when one produced this tx.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// Which surface produced this transaction. Absent on records written
    /// by releases that predate attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<TxOrigin>,
}

/// Attribution for a finalized transaction: which surface asked for it,
/// and — for agent sends — which agent, request, and canonical intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxOrigin {
    /// "cli" or "mcp".
    pub surface: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_digest: Option<String>,
}

impl TransactionRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn from_transaction(
        network: String,
        recipient: String,
        amount_sat: u64,
        fee_sat: u64,
        created_at: u64,
        excluded_utxos: u64,
        source_id: Option<String>,
        tx: &Transaction,
    ) -> Self {
        TransactionRecord {
            format_version: FORMAT_VERSION,
            txid: tx.compute_txid().to_string(),
            network,
            recipient,
            amount_sat,
            fee_sat,
            created_at,
            status: TransactionStatus::Pending,
            tx_hex: hex::encode(consensus::serialize(tx)),
            excluded_utxos,
            source_id,
            origin: None,
        }
    }

    /// Tag the record with its originating surface. A builder rather than
    /// a constructor argument so existing construction paths stay valid.
    pub fn with_origin(mut self, origin: TxOrigin) -> Self {
        self.origin = Some(origin);
        self
    }

    pub fn total_sat(&self) -> u64 {
        self.amount_sat.saturating_add(self.fee_sat)
    }

    pub fn tx(&self) -> Result<Transaction, PlanError> {
        if self.format_version != FORMAT_VERSION {
            return Err(PlanError::Transaction(format!(
                "unsupported transaction record version {}",
                self.format_version
            )));
        }
        let bytes = hex::decode(&self.tx_hex)
            .map_err(|e| PlanError::Transaction(format!("invalid transaction hex: {e}")))?;
        let tx: Transaction = consensus::deserialize(&bytes)
            .map_err(|e| PlanError::Transaction(format!("invalid transaction: {e}")))?;
        let actual = tx.compute_txid().to_string();
        if actual != self.txid {
            return Err(PlanError::Transaction(format!(
                "transaction id mismatch: record has {}, transaction has {actual}",
                self.txid
            )));
        }
        Ok(tx)
    }

    pub fn mark_broadcast(&mut self) {
        self.status = TransactionStatus::Broadcast;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deserialized state is untrusted: extreme values must not overflow.
    #[test]
    fn total_saturates_on_deserialized_extremes() {
        let session = PsbtSession {
            format_version: FORMAT_VERSION,
            id: "x".into(),
            network: "signet".into(),
            recipient: "tb1p".into(),
            amount_sat: u64::MAX,
            fee_sat: 1,
            created_at: 0,
            psbt: String::new(),
            excluded_utxos: 0,
        };
        assert_eq!(session.total_sat(), u64::MAX);

        let record = TransactionRecord {
            format_version: FORMAT_VERSION,
            txid: String::new(),
            network: "signet".into(),
            recipient: "tb1p".into(),
            amount_sat: 1,
            fee_sat: u64::MAX,
            created_at: 0,
            status: TransactionStatus::Pending,
            tx_hex: String::new(),
            excluded_utxos: 0,
            source_id: None,
            origin: None,
        };
        assert_eq!(record.total_sat(), u64::MAX);
    }

    #[test]
    fn origin_round_trips_and_defaults_to_none() {
        let record = TransactionRecord {
            format_version: FORMAT_VERSION,
            txid: String::new(),
            network: "signet".into(),
            recipient: "tb1p".into(),
            amount_sat: 1,
            fee_sat: 1,
            created_at: 0,
            status: TransactionStatus::Pending,
            tx_hex: String::new(),
            excluded_utxos: 0,
            source_id: None,
            origin: None,
        }
        .with_origin(TxOrigin {
            surface: "mcp".into(),
            agent: Some("claude".into()),
            request_id: Some("k-job-1".into()),
            intent_digest: Some("ab".repeat(32)),
        });
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["origin"]["surface"], "mcp");
        assert_eq!(json["origin"]["agent"], "claude");
        let back: TransactionRecord = serde_json::from_value(json).unwrap();
        assert_eq!(back.origin, record.origin);

        // Records written before attribution existed read as origin: None.
        let old = serde_json::json!({
            "txid": "", "network": "signet", "recipient": "tb1p",
            "amount_sat": 1, "fee_sat": 1, "created_at": 0,
            "status": "pending", "tx_hex": "",
        });
        let old: TransactionRecord = serde_json::from_value(old).unwrap();
        assert!(old.origin.is_none());
        let json = serde_json::to_value(&old).unwrap();
        assert!(
            json.get("origin").is_none(),
            "absent origin is not serialized"
        );
    }
}

/// The pre-refactor persisted plan format. It remains readable so signed
/// transactions and explicit unsigned sessions created by older releases can
/// migrate without retaining a signed PSBT in new state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyPlan {
    pub id: String,
    pub network: String,
    pub recipient: String,
    pub amount_sat: u64,
    pub fee_sat: u64,
    pub created_at: u64,
    pub status: LegacyPlanStatus,
    pub psbt: String,
    #[serde(default)]
    pub excluded_utxos: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyPlanStatus {
    Unsigned,
    Signed,
    Broadcast,
}

impl LegacyPlan {
    pub fn into_prepared(self) -> Result<PreparedSpend, PlanError> {
        let psbt = Psbt::from_str(&self.psbt).map_err(|e| PlanError::Psbt(e.to_string()))?;
        Ok(PreparedSpend::new(
            self.network,
            self.recipient,
            self.amount_sat,
            self.fee_sat,
            self.created_at,
            self.excluded_utxos,
            psbt,
        ))
    }

    pub fn into_transaction(self) -> Result<TransactionRecord, PlanError> {
        let source_id = self.id.clone();
        let psbt = Psbt::from_str(&self.psbt).map_err(|e| PlanError::Psbt(e.to_string()))?;
        let tx = psbt
            .extract_tx()
            .map_err(|e| PlanError::Extract(e.to_string()))?;
        Ok(TransactionRecord::from_transaction(
            self.network,
            self.recipient,
            self.amount_sat,
            self.fee_sat,
            self.created_at,
            self.excluded_utxos,
            Some(source_id),
            &tx,
        ))
    }
}
