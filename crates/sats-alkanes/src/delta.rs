//! Tolerant interpretation of simulation results.
//!
//! The view endpoint's result shape is not a contract sats depends on:
//! whatever matches the known fields is parsed, and the raw value is
//! always preserved for display. Parsing never fails — an unknown shape
//! is simply an opaque result.

use serde::Serialize;

use crate::id::AlkaneId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AssetDelta {
    pub id: AlkaneId,
    pub value: u128,
}

#[derive(Debug, Clone, Serialize)]
pub struct SimulationView {
    /// Execution status when the result exposes one (0 = success by
    /// convention).
    pub status: Option<u64>,
    pub gas_used: Option<u64>,
    /// Asset transfers the result reports where the shape is recognized.
    pub transfers: Vec<AssetDelta>,
    /// The verbatim result, always.
    pub raw: serde_json::Value,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SimulationError {
    #[error("refusing execution: simulation did not report a recognized numeric success status")]
    Unrecognized,
    #[error("refusing execution: simulation reported failure status {0}")]
    Failed(u64),
    #[error("refusing execution: simulation reported an error")]
    ReportedError,
}

impl SimulationView {
    /// Execution accepts only the currently recognized numeric status-0
    /// result, with no contradictory status or reported error in the known
    /// result containers. This is an endpoint claim, never proof of effects.
    /// Read-only callers may continue displaying any raw result.
    pub fn require_success(&self) -> Result<(), SimulationError> {
        for node in std::iter::once(&self.raw).chain(
            ["execution", "result"]
                .iter()
                .filter_map(|key| self.raw.get(key)),
        ) {
            if node
                .get("error")
                .is_some_and(|error| !error.is_null() && error.as_str() != Some(""))
            {
                return Err(SimulationError::ReportedError);
            }
            if let Some(status) = node.get("status") {
                match status.as_u64() {
                    Some(0) => {}
                    Some(status) => return Err(SimulationError::Failed(status)),
                    None => return Err(SimulationError::Unrecognized),
                }
            }
        }
        match self.raw.get("status").and_then(serde_json::Value::as_u64) {
            Some(0) => Ok(()),
            Some(status) => Err(SimulationError::Failed(status)),
            None => Err(SimulationError::Unrecognized),
        }
    }
}

pub fn parse_simulation(raw: &serde_json::Value) -> SimulationView {
    let status = raw.get("status").and_then(serde_json::Value::as_u64);
    let gas_used = raw
        .get("gasUsed")
        .or_else(|| raw.get("gas_used"))
        .and_then(serde_json::Value::as_u64);
    let transfers = ["execution", "result"]
        .iter()
        .filter_map(|key| raw.get(key))
        .chain(std::iter::once(raw))
        .filter_map(|node| node.get("alkanes").and_then(serde_json::Value::as_array))
        .flatten()
        .filter_map(parse_transfer)
        .collect();
    SimulationView {
        status,
        gas_used,
        transfers,
        raw: raw.clone(),
    }
}

fn parse_transfer(entry: &serde_json::Value) -> Option<AssetDelta> {
    let id = entry.get("id")?;
    Some(AssetDelta {
        id: AlkaneId {
            block: parse_u128(id.get("block")?)?,
            tx: parse_u128(id.get("tx")?)?,
        },
        value: parse_u128(entry.get("value")?)?,
    })
}

/// Numbers or strings — endpoints render u128s as decimal or 0x hex
/// strings when they exceed JSON number range.
fn parse_u128(value: &serde_json::Value) -> Option<u128> {
    if let Some(n) = value.as_u64() {
        return Some(u128::from(n));
    }
    let s = value.as_str()?.trim();
    if let Some(hex_str) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u128::from_str_radix(hex_str, 16).ok()
    } else {
        s.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_requires_unambiguous_recognized_success() {
        use serde_json::json;
        for raw in [
            json!({"status": 0}),
            json!({"status": 0, "error": null}),
            json!({"status": 0, "execution": {"error": "", "status": 0}}),
        ] {
            assert_eq!(parse_simulation(&raw).require_success(), Ok(()));
        }
        for raw in [
            json!(null),
            json!({}),
            json!({"status": "0"}),
            json!({"status": -1}),
            json!({"status": true}),
            json!({"result": {"status": 0}}),
        ] {
            assert_eq!(
                parse_simulation(&raw).require_success(),
                Err(SimulationError::Unrecognized)
            );
        }
        for raw in [
            json!({"status": 1}),
            json!({"status": 0, "execution": {"status": 1}}),
        ] {
            assert_eq!(
                parse_simulation(&raw).require_success(),
                Err(SimulationError::Failed(1))
            );
        }
        for raw in [
            json!({"status": 0, "error": "revert"}),
            json!({"status": 0, "execution": {"error": {"message": "revert"}}}),
            json!({"status": 0, "result": {"error": "revert"}}),
        ] {
            assert_eq!(
                parse_simulation(&raw).require_success(),
                Err(SimulationError::ReportedError)
            );
        }
    }

    #[test]
    fn parses_recognized_transfer_shapes() {
        let raw = serde_json::json!({
            "status": 0,
            "gasUsed": 82_500,
            "execution": {
                "alkanes": [
                    { "id": { "block": 2, "tx": 1 }, "value": "0xde0b6b3a7640000" },
                    { "id": { "block": "2", "tx": "78528" }, "value": 50 },
                ],
            },
        });
        let view = parse_simulation(&raw);
        assert_eq!(view.status, Some(0));
        assert_eq!(view.gas_used, Some(82_500));
        assert_eq!(view.transfers.len(), 2);
        assert_eq!(view.transfers[0].value, 1_000_000_000_000_000_000);
        assert_eq!(view.transfers[1].id.tx, 78_528);
    }

    #[test]
    fn unknown_shapes_stay_opaque_but_displayed() {
        let raw = serde_json::json!({ "something": ["else", 42] });
        let view = parse_simulation(&raw);
        assert_eq!(view.status, None);
        assert!(view.transfers.is_empty());
        assert_eq!(view.raw, raw);
    }

    #[test]
    fn malformed_transfer_entries_are_skipped_not_errors() {
        let raw = serde_json::json!({
            "alkanes": [
                { "id": { "block": 2 }, "value": 1 },
                { "value": 1 },
                { "id": { "block": 2, "tx": 1 }, "value": "not-a-number" },
                { "id": { "block": 2, "tx": 1 }, "value": 7 },
            ],
        });
        assert_eq!(parse_simulation(&raw).transfers.len(), 1);
    }
}
