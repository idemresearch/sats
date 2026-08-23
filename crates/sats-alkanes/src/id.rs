//! Alkane identity: the `block:tx` pair naming a contract or token.
//!
//! Reference: `crates/alkanes-support/src/id.rs` in alkanes-rs @ 62511e9
//! (`AlkaneId { block: u128, tx: u128 }`).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AlkaneId {
    pub block: u128,
    pub tx: u128,
}

impl fmt::Display for AlkaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.block, self.tx)
    }
}

impl FromStr for AlkaneId {
    type Err = String;

    /// `BLOCK:TX`, both decimal.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (block, tx) = s
            .split_once(':')
            .ok_or_else(|| format!("invalid alkane id {s:?} (expected BLOCK:TX, e.g. 2:1)"))?;
        let parse = |part: &str, name: &str| -> Result<u128, String> {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("invalid alkane id {s:?}: {name} must be decimal"));
            }
            part.parse()
                .map_err(|_| format!("invalid alkane id {s:?}: {name} is out of range"))
        };
        Ok(AlkaneId {
            block: parse(block, "block")?,
            tx: parse(tx, "tx")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_renders() {
        let id: AlkaneId = "2:78528".parse().unwrap();
        assert_eq!(
            id,
            AlkaneId {
                block: 2,
                tx: 78_528
            }
        );
        assert_eq!(id.to_string(), "2:78528");
        let max: AlkaneId = format!("{0}:{0}", u128::MAX).parse().unwrap();
        assert_eq!(max.block, u128::MAX);
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "",
            ":",
            "2",
            "2:",
            ":1",
            "2:1:3",
            "0x2:1",
            "2:-1",
            "a:b",
            "2: 1",
            "340282366920938463463374607431768211456:1", // u128::MAX + 1
        ] {
            assert!(bad.parse::<AlkaneId>().is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn json_round_trip() {
        let id = AlkaneId { block: 2, tx: 1 };
        let json = serde_json::to_value(id).unwrap();
        assert_eq!(json["block"], 2);
        let back: AlkaneId = serde_json::from_value(json).unwrap();
        assert_eq!(back, id);
    }
}
