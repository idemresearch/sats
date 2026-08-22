//! BIP-39 mnemonic → BIP-86 single-key taproot wallet descriptors.
//!
//! The persisted wallet is always watch-only (public descriptors). Private
//! keys exist only inside an ephemeral signing wallet built on demand.

use bdk_wallet::bitcoin::Network;
use bdk_wallet::bitcoin::bip32::Xpriv;
use bdk_wallet::template::Bip86;
use bdk_wallet::{KeychainKind, Wallet};
use bip39::{Language, Mnemonic};
use zeroize::Zeroizing;

use crate::error::SeedError;

/// Generate a fresh mnemonic of 12 or 24 words from OS entropy.
pub fn generate_mnemonic(words: u8) -> Result<Mnemonic, SeedError> {
    let entropy_len = match words {
        12 => 16,
        24 => 32,
        _ => return Err(SeedError::BadWordCount),
    };
    let mut entropy = Zeroizing::new(vec![0u8; entropy_len]);
    getrandom::fill(&mut entropy).map_err(|_| SeedError::Rng)?;
    Ok(Mnemonic::from_entropy(&entropy)?)
}

/// Parse a stored mnemonic string (English wordlist).
pub fn parse_mnemonic(s: &str) -> Result<Mnemonic, SeedError> {
    Ok(Mnemonic::parse_in_normalized(Language::English, s.trim())?)
}

/// A human-actionable reason a backup phrase failed to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MnemonicProblem {
    /// 1-based position of a word that is not on the BIP-39 English list.
    UnknownWord(usize),
    /// The phrase has this many words; a backup has 12, 15, 18, 21, or 24.
    WordCount(usize),
    /// Every word is valid but the checksum fails: one word is wrong or
    /// two are swapped.
    Checksum,
}

/// Classify a [`parse_mnemonic`] failure for user-facing guidance.
pub fn mnemonic_problem(err: &SeedError) -> Option<MnemonicProblem> {
    match err {
        SeedError::Mnemonic(e) => match e {
            bip39::Error::UnknownWord(index) => Some(MnemonicProblem::UnknownWord(index + 1)),
            bip39::Error::BadWordCount(count) => Some(MnemonicProblem::WordCount(*count)),
            bip39::Error::InvalidChecksum => Some(MnemonicProblem::Checksum),
            _ => None,
        },
        _ => None,
    }
}

/// Ephemeral wallet holding private keys, for signing only. Never persisted.
pub fn signing_wallet(mnemonic: &Mnemonic, network: Network) -> Result<Wallet, SeedError> {
    let seed = Zeroizing::new(mnemonic.to_seed(""));
    let xprv = Xpriv::new_master(network, seed.as_ref())?;
    Ok(Wallet::create(
        Bip86(xprv, KeychainKind::External),
        Bip86(xprv, KeychainKind::Internal),
    )
    .network(network)
    .create_wallet_no_persist()?)
}

/// Public (watch-only) descriptor strings: `(external, internal)`.
pub fn public_descriptors(
    mnemonic: &Mnemonic,
    network: Network,
) -> Result<(String, String), SeedError> {
    let wallet = signing_wallet(mnemonic, network)?;
    Ok((
        wallet.public_descriptor(KeychainKind::External).to_string(),
        wallet.public_descriptor(KeychainKind::Internal).to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The BIP-86 reference vectors.
    const VECTOR_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn bip86_mainnet_vectors() {
        let mnemonic = parse_mnemonic(VECTOR_MNEMONIC).unwrap();
        let mut wallet = signing_wallet(&mnemonic, Network::Bitcoin).unwrap();
        assert_eq!(
            wallet
                .reveal_next_address(KeychainKind::External)
                .to_string(),
            "bc1p5cyxnuxmeuwuvkwfem96lqzszd02n6xdcjrs20cac6yqjjwudpxqkedrcr"
        );
        assert_eq!(
            wallet
                .reveal_next_address(KeychainKind::External)
                .to_string(),
            "bc1p4qhjn9zdvkux4e44uhx8tc55attvtyu358kutcqkudyccelu0was9fqzwh"
        );
        assert_eq!(
            wallet
                .reveal_next_address(KeychainKind::Internal)
                .to_string(),
            "bc1p3qkhfews2uk44qtvauqyr2ttdsw7svhkl9nkm9s9c3x4ax5h60wqwruhk7"
        );
    }

    #[test]
    fn public_descriptors_are_watch_only_taproot() {
        let mnemonic = parse_mnemonic(VECTOR_MNEMONIC).unwrap();
        let (ext, int) = public_descriptors(&mnemonic, Network::Signet).unwrap();
        for desc in [&ext, &int] {
            assert!(
                desc.starts_with("tr("),
                "expected taproot descriptor: {desc}"
            );
            assert!(
                desc.contains("/86'/1'/0'"),
                "expected signet BIP-86 path: {desc}"
            );
            assert!(
                !desc.contains("prv"),
                "public descriptor leaked a private key"
            );
        }
        assert!(ext.contains("/0/*"));
        assert!(int.contains("/1/*"));
    }

    #[test]
    fn watch_only_wallet_derives_same_addresses_as_signing_wallet() {
        let mnemonic = parse_mnemonic(VECTOR_MNEMONIC).unwrap();
        let (ext, int) = public_descriptors(&mnemonic, Network::Bitcoin).unwrap();
        let mut watch = Wallet::create(ext, int)
            .network(Network::Bitcoin)
            .create_wallet_no_persist()
            .unwrap();
        assert_eq!(
            watch
                .reveal_next_address(KeychainKind::External)
                .to_string(),
            "bc1p5cyxnuxmeuwuvkwfem96lqzszd02n6xdcjrs20cac6yqjjwudpxqkedrcr"
        );
    }

    #[test]
    fn mnemonic_problems_are_classified() {
        let unknown = parse_mnemonic("abandon abandon zzzz abandon abandon abandon abandon abandon abandon abandon abandon about").unwrap_err();
        assert_eq!(
            mnemonic_problem(&unknown),
            Some(MnemonicProblem::UnknownWord(3))
        );

        let count = parse_mnemonic("abandon abandon abandon").unwrap_err();
        assert_eq!(
            mnemonic_problem(&count),
            Some(MnemonicProblem::WordCount(3))
        );

        let checksum = parse_mnemonic(&["abandon"; 12].join(" ")).unwrap_err();
        assert_eq!(mnemonic_problem(&checksum), Some(MnemonicProblem::Checksum));

        assert_eq!(mnemonic_problem(&SeedError::BadWordCount), None);
    }

    #[test]
    fn generate_word_counts() {
        assert_eq!(generate_mnemonic(12).unwrap().word_count(), 12);
        assert_eq!(generate_mnemonic(24).unwrap().word_count(), 24);
        assert!(matches!(
            generate_mnemonic(15),
            Err(SeedError::BadWordCount)
        ));
        // Two generations must differ (entropy actually used).
        assert_ne!(
            generate_mnemonic(12).unwrap().to_string(),
            generate_mnemonic(12).unwrap().to_string()
        );
    }
}
