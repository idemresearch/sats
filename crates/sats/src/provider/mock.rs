//! File-backed mock driver: a deterministic, network-free provider for the
//! integration tests. Deliberately undocumented: any `file:///abs/dir`
//! endpoint (an Esplora `url`, a `subfrost_url`) selects it, and the
//! directory controls behavior:
//!
//! - `sync-error`      present → sync fails with the file's contents
//! - `fees.json`       `{"2": 3.0}` conf-target → sat/vB map (default: flat 2)
//! - `broadcasts.log`  broadcast txids are appended here
//! - `alkanes-bytecode.json`  `{"BLOCK:TX": "<hex>"}`; a missing file or
//!   key is a typed view error
//! - `alkanes-simulate.json`  returned verbatim for any simulate call;
//!   missing file is a typed view error
//! - `fees-via`, `broadcast-via`  an Esplora URL that serves that one role
//!   instead, so tests reach the real HTTP transport behind a
//!   deterministic sync

use std::collections::HashMap;
use std::path::PathBuf;

use sats_core::bitcoin::{Transaction, Txid};

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

    pub fn display_url(&self) -> &str {
        &self.display
    }

    /// The Esplora URL in a `fees-via` or `broadcast-via` file, if any.
    pub fn delegate(&self, role: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join(role))
            .ok()
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty())
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
        let marker = self.dir.join("broadcast-fail");
        if marker.exists() {
            let message = std::fs::read_to_string(&marker)
                .unwrap_or_default()
                .trim()
                .to_string();
            return Err(ProviderError::Broadcast {
                url: self.display.clone(),
                message: if message.is_empty() {
                    "mock broadcast error".into()
                } else {
                    message
                },
            });
        }
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

    fn view_err(&self, message: String) -> ProviderError {
        ProviderError::View {
            url: self.display.clone(),
            message,
        }
    }

    pub fn alkanes_bytecode(&self, block: u128, tx: u128) -> Result<Vec<u8>, ProviderError> {
        let text = std::fs::read_to_string(self.dir.join("alkanes-bytecode.json"))
            .map_err(|e| self.view_err(e.to_string()))?;
        let map: HashMap<String, String> =
            serde_json::from_str(&text).map_err(|e| self.view_err(e.to_string()))?;
        let key = format!("{block}:{tx}");
        let hex_str = map
            .get(&key)
            .ok_or_else(|| self.view_err(format!("no bytecode for {key}")))?;
        hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))
            .map_err(|e| self.view_err(e.to_string()))
    }

    pub fn alkanes_simulate(&self) -> Result<serde_json::Value, ProviderError> {
        let text = std::fs::read_to_string(self.dir.join("alkanes-simulate.json"))
            .map_err(|e| self.view_err(e.to_string()))?;
        serde_json::from_str(&text).map_err(|e| self.view_err(e.to_string()))
    }
}
