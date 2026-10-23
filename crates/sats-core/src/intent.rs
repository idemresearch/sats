//! Canonical, deterministic form of an agent's intent, and its digest.
//!
//! The digest is what one-time approvals bind to and what retry
//! deduplication compares: two requests mean the same thing exactly when
//! their digests match. PURE — no IO, no clock; callers normalize the
//! recipient (parse, require the network, re-render) before hashing so
//! textual address variants collapse to one digest.

use bdk_wallet::bitcoin::hashes::{Hash, sha256};
use serde::{Deserialize, Serialize};

/// Domain-separation tag for the digest. Bump only with a new digest
/// version; every stored digest becomes unmatchable when this changes.
pub const INTENT_DIGEST_DOMAIN: &[u8] = b"sats-intent-v1";

/// Public contract facts bound to a single frBTC wrapping request. No key
/// material: the address is the public BTC payment destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrbtcContract {
    pub signer_address: String,
    pub code_hash: String,
}

/// A protocol-qualified identity. Protocol adapters normalize ids; the
/// common request model does not interpret names or protocol encodings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetId {
    pub protocol: String,
    pub id: String,
}

/// Exact asset quantity, independently of any Bitcoin funding amount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetTransfer {
    pub asset: AssetId,
    #[serde(with = "positive_decimal")]
    pub amount: u128,
}

mod positive_decimal {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    pub fn serialize<S: Serializer>(amount: &u128, serializer: S) -> Result<S::Ok, S::Error> {
        if *amount == 0 {
            return Err(serde::ser::Error::custom("asset amount must be positive"));
        }
        serializer.serialize_str(&amount.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u128, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(D::Error::custom(
                "asset amount must be a positive decimal u128 string",
            ));
        }
        value
            .parse::<u128>()
            .ok()
            .filter(|v| *v > 0)
            .ok_or_else(|| D::Error::custom("asset amount must be a positive decimal u128 string"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RequestOperation {
    #[default]
    Send,
    FrbtcWrap {
        contract: FrbtcContract,
    },
    AssetTransfer {
        transfer: AssetTransfer,
    },
}

impl RequestOperation {
    pub fn is_send(&self) -> bool {
        matches!(self, Self::Send)
    }
    pub fn name(&self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::FrbtcWrap { .. } => "frbtc_wrap",
            Self::AssetTransfer { .. } => "asset_transfer",
        }
    }
}

/// The canonical facts of a send: who asks, on which network, to whom,
/// how much. The fee is deliberately absent — it varies per preparation,
/// and a retry of the same logical send must produce the same digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendIntent {
    pub network: String,
    pub agent: String,
    /// Normalized recipient address string.
    pub recipient: String,
    pub amount_sat: u64,
}

impl SendIntent {
    /// Bind operation-specific facts as well as the common BTC payment.
    /// Asset identity/quantity and wrapping contract facts use separate domains.
    pub fn digest_for(&self, operation: &RequestOperation) -> String {
        if let RequestOperation::AssetTransfer { transfer } = operation {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(b"sats-asset-transfer-v1\0");
            encode_str(&mut bytes, &self.network);
            encode_str(&mut bytes, &self.agent);
            encode_str(&mut bytes, &self.recipient);
            bytes.extend_from_slice(&self.amount_sat.to_le_bytes());
            encode_str(&mut bytes, &transfer.asset.protocol);
            encode_str(&mut bytes, &transfer.asset.id);
            bytes.extend_from_slice(&transfer.amount.to_le_bytes());
            return sha256::Hash::hash(&bytes).to_string();
        }
        let RequestOperation::FrbtcWrap { contract } = operation else {
            return self.digest();
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"sats-frbtc-wrap-v1\0");
        encode_str(&mut bytes, &self.network);
        encode_str(&mut bytes, &self.agent);
        encode_str(&mut bytes, &self.recipient);
        bytes.extend_from_slice(&self.amount_sat.to_le_bytes());
        encode_str(&mut bytes, "32:0:77:wallet");
        encode_str(&mut bytes, &contract.signer_address);
        encode_str(&mut bytes, &contract.code_hash);
        sha256::Hash::hash(&bytes).to_string()
    }
    /// 64-char lowercase hex sha256 over a length-prefixed encoding:
    /// `DOMAIN || enc("send") || enc(network) || enc(agent) ||
    /// enc(recipient) || u64_le(amount_sat)` with
    /// `enc(s) = u64_le(s.len()) || s`. Length prefixes make the
    /// encoding injective: no field can smear into its neighbor.
    pub fn digest(&self) -> String {
        let mut bytes = Vec::with_capacity(
            INTENT_DIGEST_DOMAIN.len()
                + 5 * 8
                + 4
                + self.network.len()
                + self.agent.len()
                + self.recipient.len(),
        );
        bytes.extend_from_slice(INTENT_DIGEST_DOMAIN);
        encode_str(&mut bytes, "send");
        encode_str(&mut bytes, &self.network);
        encode_str(&mut bytes, &self.agent);
        encode_str(&mut bytes, &self.recipient);
        bytes.extend_from_slice(&self.amount_sat.to_le_bytes());
        sha256::Hash::hash(&bytes).to_string()
    }
}

fn encode_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u64).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent() -> SendIntent {
        SendIntent {
            network: "signet".into(),
            agent: "claude".into(),
            recipient: "tb1pexampleaddress0000".into(),
            amount_sat: 25_000,
        }
    }

    #[test]
    fn digest_is_stable() {
        // Freezes the encoding: any change to the domain tag, field
        // order, or length-prefix scheme must fail this test.
        assert_eq!(
            intent().digest(),
            "5e3a281c87ab8ab420d304157ebff535c024a30869a7d25abdec3ff60f93198f"
        );
    }

    #[test]
    fn digest_separates_fields() {
        // Without length prefixes these two would hash identical bytes.
        let a = SendIntent {
            network: "signet".into(),
            agent: "ab".into(),
            recipient: "c".into(),
            amount_sat: 1,
        };
        let b = SendIntent {
            network: "signet".into(),
            agent: "a".into(),
            recipient: "bc".into(),
            amount_sat: 1,
        };
        assert_ne!(a.digest(), b.digest());
        assert_eq!(
            a.digest(),
            "39dfcf35494e9dd971520d6d68bb5fd3b135f301f517be127dd445caeffbbe65"
        );
        assert_eq!(
            b.digest(),
            "ebbc5013b0f72e5ece1c62d468821256923f2300f4abac1d30bc9a8af99b963b"
        );
    }

    #[test]
    fn digest_changes_with_each_field() {
        let base = intent().digest();
        let mut network = intent();
        network.network = "testnet4".into();
        let mut agent = intent();
        agent.agent = "other".into();
        let mut recipient = intent();
        recipient.recipient = "tb1pother".into();
        let mut amount = intent();
        amount.amount_sat = 25_001;
        for changed in [network, agent, recipient, amount] {
            assert_ne!(changed.digest(), base);
        }
    }

    #[test]
    fn wrapping_is_bound_to_kind_contract_and_pinned_facts() {
        let send = intent();
        let contract = FrbtcContract {
            signer_address: send.recipient.clone(),
            code_hash: "ab".repeat(32),
        };
        let operation = RequestOperation::FrbtcWrap {
            contract: contract.clone(),
        };
        let digest = send.digest_for(&operation);
        assert_ne!(digest, send.digest());
        let mut changed = contract;
        changed.code_hash = "cd".repeat(32);
        assert_ne!(
            digest,
            send.digest_for(&RequestOperation::FrbtcWrap { contract: changed })
        );
        let mut changed = send.clone();
        changed.amount_sat += 1;
        assert_ne!(digest, changed.digest_for(&operation));
        changed = send.clone();
        changed.network = "testnet4".into();
        assert_ne!(digest, changed.digest_for(&operation));
    }

    #[test]
    fn digest_is_lowercase_hex_64() {
        let d = intent().digest();
        assert_eq!(d.len(), 64);
        assert!(
            d.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }
}

#[cfg(test)]
mod asset_tests {
    use super::*;

    fn intent() -> SendIntent {
        SendIntent {
            network: "testnet4".into(),
            agent: "ai".into(),
            recipient: "recipient".into(),
            amount_sat: 546,
        }
    }
    fn transfer() -> RequestOperation {
        RequestOperation::AssetTransfer {
            transfer: AssetTransfer {
                asset: AssetId {
                    protocol: "alkanes".into(),
                    id: "32:0".into(),
                },
                amount: u128::MAX,
            },
        }
    }

    #[test]
    fn asset_digest_binds_every_fact_and_is_separate_from_btc_and_wrap() {
        let base = intent();
        let op = transfer();
        let digest = base.digest_for(&op);
        assert_ne!(digest, base.digest());
        assert_ne!(
            digest,
            base.digest_for(&RequestOperation::FrbtcWrap {
                contract: FrbtcContract {
                    signer_address: "recipient".into(),
                    code_hash: "hash".into()
                }
            })
        );
        for field in ["network", "agent", "recipient", "postage"] {
            let mut changed = base.clone();
            match field {
                "network" => changed.network = "signet".into(),
                "agent" => changed.agent = "other".into(),
                "recipient" => changed.recipient = "other".into(),
                _ => changed.amount_sat += 1,
            }
            assert_ne!(digest, changed.digest_for(&op), "{field}");
        }
        for field in ["protocol", "id", "quantity"] {
            let mut changed = op.clone();
            let RequestOperation::AssetTransfer { transfer } = &mut changed else {
                unreachable!()
            };
            match field {
                "protocol" => transfer.asset.protocol = "other".into(),
                "id" => transfer.asset.id = "2:0".into(),
                _ => transfer.amount -= 1,
            }
            assert_ne!(digest, base.digest_for(&changed), "{field}");
        }
    }

    #[test]
    fn exact_asset_quantity_is_a_positive_decimal_string() {
        let op = transfer();
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(json["transfer"]["amount"], u128::MAX.to_string());
        assert_eq!(
            serde_json::from_value::<RequestOperation>(json.clone()).unwrap(),
            op
        );
        for bad in [
            serde_json::json!(0),
            serde_json::json!(1),
            serde_json::json!("0"),
            serde_json::json!("-1"),
            serde_json::json!("+1"),
            serde_json::json!("1.5"),
            serde_json::json!(""),
            serde_json::json!("340282366920938463463374607431768211456"),
        ] {
            let mut changed = json.clone();
            changed["transfer"]["amount"] = bad;
            assert!(serde_json::from_value::<RequestOperation>(changed).is_err());
        }
    }
}
