//! Subfrost driver: one aggregate JSON-RPC endpoint multiplexing
//! esplora-style chain queries with ord and alkanes indexer queries
//! (sandshrew-compatible namespacing).
//!
//! The URL may carry an API key in its path
//! (`https://mainnet.subfrost.io/v4/<KEY>/jsonrpc`), so it is a secret:
//! every error and log line uses the redacted origin-only form.

use std::collections::HashMap;

use sats_core::bitcoin::{OutPoint, Transaction, Txid, consensus};
use serde::de::DeserializeOwned;

use super::error::{ProviderError, redact_url};

/// The wire dialect: sandshrew-compatible namespaced methods, where
/// `esplora_*` mirrors the Esplora REST paths.
///
/// TODO(subfrost-dialect): confirm every method name and result shape
/// against a live endpoint (the docs sites are unreachable from this
/// development environment). The dialect is deliberately confined to this
/// table and the `parse_*` helpers below so corrections stay local.
mod dialect {
    pub const FEE_ESTIMATES: &str = "esplora_fee-estimates";
    pub const BROADCAST: &str = "esplora_broadcast";
    pub const ORD_OUTPUT: &str = "ord_output";
    pub const ALKANES_BY_OUTPOINT: &str = "alkanes_protorunesbyoutpoint";
}

#[derive(Clone)]
pub struct SubfrostClient {
    url: String,
    display_url: String,
}

/// Manual Debug: the path key must never reach a log line.
impl std::fmt::Debug for SubfrostClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubfrostClient")
            .field("url", &self.display_url)
            .finish_non_exhaustive()
    }
}

impl SubfrostClient {
    pub fn new(url: String) -> Self {
        let display_url = redact_url(&url);
        SubfrostClient { url, display_url }
    }

    pub fn display_url(&self) -> &str {
        &self.display_url
    }

    /// Scrub the full URL (which may carry a path key) out of transport
    /// error text before it can reach a user-visible string.
    fn scrub(&self, text: &str) -> String {
        text.replace(&self.url, &self.display_url)
    }

    fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T, String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": method,
            "params": params,
        });
        let response = minreq::post(&self.url)
            .with_header("Content-Type", "application/json")
            .with_json(&body)
            .map_err(|e| self.scrub(&e.to_string()))?
            .send()
            .map_err(|e| self.scrub(&e.to_string()))?;
        if !(200..300).contains(&response.status_code) {
            return Err(format!("http {}", response.status_code));
        }
        let text = response.as_str().map_err(|e| self.scrub(&e.to_string()))?;
        parse_jsonrpc(text)
    }

    pub fn fee_estimates(&self) -> Result<HashMap<u16, f64>, ProviderError> {
        self.call(dialect::FEE_ESTIMATES, serde_json::json!([]))
            .map_err(|message| ProviderError::Fees {
                url: self.display_url.clone(),
                message,
            })
    }

    pub fn broadcast(&self, tx: &Transaction) -> Result<Txid, ProviderError> {
        let hex = hex::encode(consensus::encode::serialize(tx));
        let txid = tx.compute_txid();
        let echoed: String = self
            .call(dialect::BROADCAST, serde_json::json!([hex]))
            .map_err(|message| ProviderError::Broadcast {
                url: self.display_url.clone(),
                message,
            })?;
        if echoed.trim() != txid.to_string() {
            return Err(ProviderError::Broadcast {
                url: self.display_url.clone(),
                message: format!("endpoint echoed unexpected txid {:?}", echoed.trim()),
            });
        }
        Ok(txid)
    }

    fn guard_err(&self, name: &'static str, message: String) -> ProviderError {
        ProviderError::Guard {
            name: name.to_string(),
            url: self.display_url.clone(),
            message,
        }
    }

    /// Which of these outpoints carry ord assets (inscriptions or runes)?
    pub fn ord_protected(&self, outpoints: &[OutPoint]) -> Result<Vec<OutPoint>, ProviderError> {
        let mut protected = Vec::new();
        for outpoint in outpoints {
            let value: serde_json::Value = self
                .call(dialect::ORD_OUTPUT, serde_json::json!([outpoint.to_string()]))
                .map_err(|m| self.guard_err("ord", m))?;
            if parse_ord_output(&value) {
                protected.push(*outpoint);
            }
        }
        Ok(protected)
    }

    /// Which of these outpoints carry alkanes balances?
    pub fn alkanes_protected(
        &self,
        outpoints: &[OutPoint],
    ) -> Result<Vec<OutPoint>, ProviderError> {
        let mut protected = Vec::new();
        for outpoint in outpoints {
            let value: serde_json::Value = self
                .call(
                    dialect::ALKANES_BY_OUTPOINT,
                    serde_json::json!([{ "txid": outpoint.txid.to_string(), "vout": outpoint.vout }]),
                )
                .map_err(|m| self.guard_err("alkanes", m))?;
            if parse_alkanes_outpoint(&value) {
                protected.push(*outpoint);
            }
        }
        Ok(protected)
    }
}

/// Unwrap a JSON-RPC 2.0 envelope. A JSON-RPC error is an error string;
/// a missing result is too.
fn parse_jsonrpc<T: DeserializeOwned>(body: &str) -> Result<T, String> {
    let envelope: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("invalid json-rpc response: {e}"))?;
    if let Some(error) = envelope.get("error").filter(|e| !e.is_null()) {
        let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(format!("rpc error {code}: {message}"));
    }
    let result = envelope
        .get("result")
        .ok_or_else(|| "json-rpc response has no result".to_string())?;
    serde_json::from_value(result.clone()).map_err(|e| format!("unexpected result shape: {e}"))
}

/// An ord `output` result marks the outpoint protected iff it lists any
/// inscriptions or runes. Opaque to sats: no decoding, only presence.
fn parse_ord_output(value: &serde_json::Value) -> bool {
    let has_inscriptions = value
        .get("inscriptions")
        .and_then(|i| i.as_array())
        .is_some_and(|a| !a.is_empty());
    let has_runes = match value.get("runes") {
        Some(serde_json::Value::Array(a)) => !a.is_empty(),
        Some(serde_json::Value::Object(o)) => !o.is_empty(),
        _ => false,
    };
    has_inscriptions || has_runes
}

/// An alkanes by-outpoint result marks the outpoint protected iff it
/// reports any balance entries.
fn parse_alkanes_outpoint(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => match o.get("balances") {
            Some(serde_json::Value::Array(a)) => !a.is_empty(),
            Some(serde_json::Value::Object(inner)) => !inner.is_empty(),
            Some(serde_json::Value::Null) | None => !o.is_empty(),
            Some(_) => true,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonrpc_result_unwraps() {
        let ok: HashMap<u16, f64> =
            parse_jsonrpc(r#"{"jsonrpc":"2.0","id":0,"result":{"2":3.5}}"#).unwrap();
        assert_eq!(ok.get(&2), Some(&3.5));
    }

    #[test]
    fn jsonrpc_error_is_reported() {
        let err = parse_jsonrpc::<serde_json::Value>(
            r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"method not found"}}"#,
        )
        .unwrap_err();
        assert!(err.contains("-32601"));
        assert!(err.contains("method not found"));
    }

    #[test]
    fn jsonrpc_null_error_is_not_an_error() {
        let ok: String =
            parse_jsonrpc(r#"{"jsonrpc":"2.0","id":0,"error":null,"result":"abc"}"#).unwrap();
        assert_eq!(ok, "abc");
    }

    #[test]
    fn ord_output_protection() {
        let empty: serde_json::Value =
            serde_json::from_str(r#"{"inscriptions":[],"runes":{}}"#).unwrap();
        assert!(!parse_ord_output(&empty));
        let inscribed: serde_json::Value =
            serde_json::from_str(r#"{"inscriptions":["abc123i0"],"runes":{}}"#).unwrap();
        assert!(parse_ord_output(&inscribed));
        let runic: serde_json::Value = serde_json::from_str(
            r#"{"inscriptions":[],"runes":{"UNCOMMON•GOODS":{"amount":420}}}"#,
        )
        .unwrap();
        assert!(parse_ord_output(&runic));
        let bare: serde_json::Value = serde_json::from_str("{}").unwrap();
        assert!(!parse_ord_output(&bare));
    }

    #[test]
    fn alkanes_outpoint_protection() {
        assert!(!parse_alkanes_outpoint(&serde_json::Value::Null));
        let empty: serde_json::Value = serde_json::from_str("[]").unwrap();
        assert!(!parse_alkanes_outpoint(&empty));
        let some: serde_json::Value =
            serde_json::from_str(r#"[{"token":{"id":"2:0"},"value":"1000"}]"#).unwrap();
        assert!(parse_alkanes_outpoint(&some));
        let object: serde_json::Value =
            serde_json::from_str(r#"{"balances":[{"id":"2:0"}]}"#).unwrap();
        assert!(parse_alkanes_outpoint(&object));
        let object_empty: serde_json::Value = serde_json::from_str(r#"{"balances":[]}"#).unwrap();
        assert!(!parse_alkanes_outpoint(&object_empty));
    }

    #[test]
    fn secrets_never_appear_in_errors() {
        let client = SubfrostClient::new("https://mainnet.subfrost.io/v4/SECRETKEY/jsonrpc".into());
        assert_eq!(client.display_url(), "https://mainnet.subfrost.io");
        assert!(!format!("{client:?}").contains("SECRETKEY"));
        let scrubbed =
            client.scrub("error connecting to https://mainnet.subfrost.io/v4/SECRETKEY/jsonrpc: refused");
        assert!(!scrubbed.contains("SECRETKEY"));
        let guard_err = client.guard_err("ord", client.scrub("boom at https://mainnet.subfrost.io/v4/SECRETKEY/jsonrpc"));
        assert!(!guard_err.to_string().contains("SECRETKEY"));
    }
}
