//! Asset names. An alkane's name and symbol are whatever its contract
//! chose, as the indexer reports them: labels, never identity. The id is
//! the asset. The one name sats knows independently is frBTC's, at the
//! fixed id the wrap flow already uses.

use crate::id::AlkaneId;
use crate::wrap::FRBTC;

/// A name sats knows from the protocol, and the only id that has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownAsset {
    pub name: &'static str,
    pub id: AlkaneId,
}

const KNOWN: &[KnownAsset] = &[KnownAsset {
    name: "frBTC",
    id: FRBTC,
}];

/// The known asset whose name the label of `id` claims, when `id` isn't
/// that asset. Case, spacing, and punctuation don't hide a claim:
/// "FR-BTC" and "Wrapped frbtc" both claim frBTC.
pub fn impersonated(id: AlkaneId, name: &str, symbol: &str) -> Option<KnownAsset> {
    let fold = |label: &str| -> String {
        label
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    let (name, symbol) = (fold(name), fold(symbol));
    KNOWN.iter().copied().find(|known| {
        let claim = fold(known.name);
        known.id != id && (name.contains(&claim) || symbol.contains(&claim))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> AlkaneId {
        s.parse().unwrap()
    }

    #[test]
    fn frbtc_may_call_itself_frbtc() {
        assert_eq!(impersonated(FRBTC, "frBTC", "FRBTC"), None);
    }

    #[test]
    fn another_id_claiming_frbtc_is_an_impostor() {
        for (name, symbol) in [
            ("frBTC", ""),
            ("", "FRBTC"),
            ("FR-BTC", ""),
            ("Wrapped frbtc", "WFR"),
            ("fr btc", ""),
        ] {
            let found = impersonated(id("2:999"), name, symbol);
            assert_eq!(found.map(|k| k.id), Some(FRBTC), "{name:?} {symbol:?}");
        }
    }

    #[test]
    fn unrelated_labels_claim_nothing() {
        for (name, symbol) in [
            ("DIESEL", "DIESEL"),
            ("", ""),
            ("BTC", "BTC"),
            ("fr", "btc"),
        ] {
            assert_eq!(impersonated(id("2:0"), name, symbol), None, "{name:?}");
        }
    }
}
