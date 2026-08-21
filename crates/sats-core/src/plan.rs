//! A plan is an unsigned-to-broadcast transaction with human-readable
//! metadata: what you're paying, what it costs, and where it stands.

use std::str::FromStr;

use bdk_wallet::bitcoin::{Psbt, Transaction};
use serde::{Deserialize, Serialize};

use crate::error::PlanError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Unsigned,
    Signed,
    Broadcast,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub network: String,
    pub recipient: String,
    pub amount_sat: u64,
    pub fee_sat: u64,
    pub created_at: u64,
    pub status: PlanStatus,
    /// The PSBT, base64-encoded. Replaced by the signed PSBT after signing.
    pub psbt: String,
    /// Wallet UTXOs excluded from coin selection (guards ∪ dust heuristic).
    /// Zero for plans saved before exclusion existed.
    #[serde(default)]
    pub excluded_utxos: u64,
}

impl Plan {
    pub fn total_sat(&self) -> u64 {
        self.amount_sat + self.fee_sat
    }

    pub fn psbt(&self) -> Result<Psbt, PlanError> {
        Psbt::from_str(&self.psbt).map_err(|e| PlanError::Psbt(e.to_string()))
    }

    pub fn set_psbt(&mut self, psbt: &Psbt) {
        self.psbt = psbt.to_string();
    }

    /// Extract the final transaction (signed plans only).
    pub fn tx(&self) -> Result<Transaction, PlanError> {
        self.psbt()?
            .extract_tx()
            .map_err(|e| PlanError::Extract(e.to_string()))
    }
}
