//! UTXO guards: external views that answer one question — of these
//! outpoints, which are carrying anything? Guards are **restrictive only**:
//! structurally, a [`GuardReport`] can only add outpoints to the unspendable
//! set. A guard never authorizes anything and never adds spendable UTXOs.
//!
//! Failure is fail-closed: a configured guard that cannot answer stops
//! planning (see `docs/metaprotocols.md`). The per-invocation escape is the
//! caller's `--no-guards`, which skips this module entirely.

use std::collections::{BTreeMap, BTreeSet};

use sats_core::bitcoin::OutPoint;

use super::error::ProviderError;
use super::mock::MockProvider;
use super::subfrost::SubfrostClient;

/// One configured guard. Audited enum, not a plugin surface.
#[derive(Debug, Clone)]
pub enum UtxoGuard {
    SubfrostOrd(SubfrostClient),
    SubfrostAlkanes(SubfrostClient),
    Mock(MockProvider),
    #[cfg(test)]
    Static(BTreeSet<OutPoint>),
}

impl UtxoGuard {
    /// Short display name for warnings ("guard: carrying assets").
    pub fn kind(&self) -> &'static str {
        match self {
            UtxoGuard::SubfrostOrd(_) => "ord",
            UtxoGuard::SubfrostAlkanes(_) => "alkanes",
            UtxoGuard::Mock(_) => "mock",
            #[cfg(test)]
            UtxoGuard::Static(_) => "static",
        }
    }

    fn protected(&self, outpoints: &[OutPoint]) -> Result<Vec<OutPoint>, ProviderError> {
        match self {
            UtxoGuard::SubfrostOrd(client) => client.ord_protected(outpoints),
            UtxoGuard::SubfrostAlkanes(client) => client.alkanes_protected(outpoints),
            UtxoGuard::Mock(mock) => mock.protected(outpoints),
            #[cfg(test)]
            UtxoGuard::Static(set) => Ok(outpoints
                .iter()
                .filter(|op| set.contains(op))
                .copied()
                .collect()),
        }
    }
}

/// What the guards said about a set of outpoints.
#[derive(Debug, Default)]
pub struct GuardReport {
    pub protected: BTreeSet<OutPoint>,
    /// Opaque display strings ("inscription", "alkanes") — shown to humans,
    /// never used in logic.
    pub kinds: BTreeMap<OutPoint, String>,
}

/// Query every guard and union the answers. Any guard failing fails the
/// whole query — an outage must not become a burned inscription.
pub fn protected_outpoints(
    guards: &[UtxoGuard],
    outpoints: &[OutPoint],
) -> Result<GuardReport, ProviderError> {
    let mut report = GuardReport::default();
    for guard in guards {
        for outpoint in guard.protected(outpoints)? {
            report.protected.insert(outpoint);
            report
                .kinds
                .entry(outpoint)
                .or_insert_with(|| guard.kind().to_string());
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn op(n: u32) -> OutPoint {
        OutPoint::from_str(&format!(
            "aa00000000000000000000000000000000000000000000000000000000000000:{n}"
        ))
        .unwrap()
    }

    #[test]
    fn union_across_guards() {
        let guards = vec![
            UtxoGuard::Static(BTreeSet::from([op(0)])),
            UtxoGuard::Static(BTreeSet::from([op(1), op(0)])),
        ];
        let report = protected_outpoints(&guards, &[op(0), op(1), op(2)]).unwrap();
        assert_eq!(report.protected, BTreeSet::from([op(0), op(1)]));
    }

    #[test]
    fn guards_only_restrict_to_queried_outpoints() {
        let guards = vec![UtxoGuard::Static(BTreeSet::from([op(7)]))];
        let report = protected_outpoints(&guards, &[op(1)]).unwrap();
        assert!(report.protected.is_empty());
    }
}
