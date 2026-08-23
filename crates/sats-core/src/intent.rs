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
        network.network = "mainnet".into();
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
    fn digest_is_lowercase_hex_64() {
        let d = intent().digest();
        assert_eq!(d.len(), 64);
        assert!(
            d.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }
}
