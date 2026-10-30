//! Asset names. An alkane's name and symbol are whatever its contract
//! chose, as the indexer reports them: labels, never identity. The id is
//! the asset. sats knows a few names independently, each pinned here to
//! the one id that has it on every network sats supports; those names may
//! stand in for the id where a person or agent types one, and nowhere else.

use crate::id::AlkaneId;
use crate::wrap::FRBTC;

/// A name sats knows from the protocol, and the only id that has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownAsset {
    pub name: &'static str,
    pub id: AlkaneId,
}

/// Adding an asset here is a reviewed change: its name then selects its
/// id in every command and request.
const KNOWN: &[KnownAsset] = &[KnownAsset {
    name: "frBTC",
    id: FRBTC,
}];

/// The known asset at `id`, if any.
pub fn known(id: AlkaneId) -> Option<KnownAsset> {
    KNOWN.iter().copied().find(|k| k.id == id)
}

/// An asset as typed: a decimal `BLOCK:TX` id, or a known name in any
/// case. Names resolve here, before anything is recorded, so requests,
/// reviews, and receipts carry only ids.
pub fn resolve(input: &str) -> Result<AlkaneId, String> {
    if input.contains(':') {
        return input.parse();
    }
    KNOWN
        .iter()
        .find(|k| k.name.eq_ignore_ascii_case(input))
        .map(|k| k.id)
        .ok_or_else(|| {
            let names: Vec<String> = KNOWN.iter().map(|k| k.name.to_lowercase()).collect();
            format!(
                "unknown asset {input:?}: give its BLOCK:TX id, e.g. 2:1 (names sats knows: {})",
                names.join(", ")
            )
        })
}

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
    fn names_resolve_to_their_ids_in_any_case() {
        for name in ["frbtc", "frBTC", "FRBTC"] {
            assert_eq!(resolve(name), Ok(FRBTC), "{name}");
        }
        assert_eq!(known(FRBTC).map(|k| k.name), Some("frBTC"));
        assert_eq!(known(id("2:0")), None);
    }

    #[test]
    fn ids_parse_as_before() {
        assert_eq!(resolve("32:0"), Ok(FRBTC));
        assert_eq!(resolve("032:00"), Ok(FRBTC));
        assert_eq!(resolve("2:78528"), Ok(id("2:78528")));
        assert!(resolve("2:x").unwrap_err().contains("invalid alkane id"));
    }

    #[test]
    fn unknown_names_ask_for_the_id() {
        for input in ["diesel", "frbtc ", "fr-btc", "", "wrapped frbtc"] {
            let err = resolve(input).unwrap_err();
            assert!(err.contains("give its BLOCK:TX id"), "{input:?}: {err}");
            assert!(err.contains("frbtc"), "{err}");
        }
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
