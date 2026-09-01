//! Transaction preparation and durable finalized-transaction records.
//!
//! A [`PreparedSpend`] exists only while a caller is reviewing, authorizing,
//! and signing a transaction. Normal sends never serialize its PSBT; an
//! explicit export writes a plain PSBT artifact instead. The durable
//! retry/audit record is always a raw finalized transaction.

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

    /// Consume a finalized PSBT and produce the only durable send record.
    pub fn into_transaction(self, signed_psbt: Psbt) -> Result<TransactionRecord, PlanError> {
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
            &tx,
        ))
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
    pub fn from_transaction(
        network: String,
        recipient: String,
        amount_sat: u64,
        fee_sat: u64,
        created_at: u64,
        excluded_utxos: u64,
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
