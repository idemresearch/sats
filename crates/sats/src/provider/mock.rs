//! File-backed mock driver: a deterministic, network-free provider for the
//! integration tests. Deliberately undocumented; `driver = "mock"` with
//! `url = "file:///abs/dir"` where the directory controls behavior:
//!
//! - `sync-error`      present → sync fails with the file's contents
//! - `fees.json`       `{"2": 3.0}` conf-target → sat/vB map (default: flat 2)
//! - `broadcasts.log`  broadcast txids are appended here
//! - `guard.json`      `{"protected": ["txid:vout", ...]}`; **missing file is
//!   a guard error** so fail-closed behavior is exercisable

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;

use sats_core::bitcoin::{OutPoint, Transaction, Txid};

use super::error::ProviderError;

#[derive(Debug, Clone)]
pub struct MockProvider {
    dir: PathBuf,
    display: String,
}

impl MockProvider {
    pub fn new(url: &str) -> Result<Self, String> {
        let path = url
            .strip_prefix("file://")
            .ok_or_else(|| "mock driver needs a file:// url".to_string())?;
        Ok(MockProvider {
            dir: PathBuf::from(path),
            display: format!("mock:{path}"),
        })
    }

    pub fn url(&self) -> &str {
        &self.display
    }

    pub fn sync(&self) -> Result<(), ProviderError> {
        let marker = self.dir.join("sync-error");
        if marker.exists() {
            let message = std::fs::read_to_string(&marker)
                .unwrap_or_default()
                .trim()
                .to_string();
            return Err(ProviderError::Sync {
                url: self.display.clone(),
                message: if message.is_empty() {
                    "mock sync error".into()
                } else {
                    message
                },
            });
        }
        Ok(())
    }

    pub fn fee_estimates(&self) -> Result<HashMap<u16, f64>, ProviderError> {
        let path = self.dir.join("fees.json");
        if !path.exists() {
            return Ok(HashMap::from([(2u16, 2.0)]));
        }
        let text = std::fs::read_to_string(&path).map_err(|e| ProviderError::Fees {
            url: self.display.clone(),
            message: e.to_string(),
        })?;
        serde_json::from_str(&text).map_err(|e| ProviderError::Fees {
            url: self.display.clone(),
            message: e.to_string(),
        })
    }

    pub fn broadcast(&self, tx: &Transaction) -> Result<Txid, ProviderError> {
        let txid = tx.compute_txid();
        let log = self.dir.join("broadcasts.log");
        let mut lines = std::fs::read_to_string(&log).unwrap_or_default();
        lines.push_str(&format!("{txid}\n"));
        std::fs::write(&log, lines).map_err(|e| ProviderError::Broadcast {
            url: self.display.clone(),
            message: e.to_string(),
        })?;
        Ok(txid)
    }

    pub fn protected(&self, outpoints: &[OutPoint]) -> Result<Vec<OutPoint>, ProviderError> {
        let guard_err = |message: String| ProviderError::Guard {
            name: "mock".into(),
            url: self.display.clone(),
            message,
        };
        let text = std::fs::read_to_string(self.dir.join("guard.json"))
            .map_err(|e| guard_err(e.to_string()))?;
        #[derive(serde::Deserialize)]
        struct GuardFile {
            protected: Vec<String>,
        }
        let file: GuardFile =
            serde_json::from_str(&text).map_err(|e| guard_err(e.to_string()))?;
        let listed = file
            .protected
            .iter()
            .map(|s| OutPoint::from_str(s))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| guard_err(e.to_string()))?;
        Ok(outpoints
            .iter()
            .filter(|op| listed.contains(op))
            .copied()
            .collect())
    }
}
