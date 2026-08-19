// Variants wrapping a `#[from]` source deliberately omit `{0}` from their
// display text: callers print the full error chain, and embedding the
// source would render it twice.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SealError {
    #[error("unsupported sealed-blob version {0}")]
    UnsupportedVersion(u32),
    #[error("unsupported kdf {0:?}")]
    UnsupportedKdf(String),
    #[error("malformed sealed blob: {0}")]
    Malformed(String),
    #[error("key derivation failed")]
    Kdf,
    #[error("randomness unavailable")]
    Rng,
    #[error("encryption failed")]
    Encrypt,
    #[error("wrong password or corrupted data")]
    Decrypt,
}

#[derive(Debug, Error)]
pub enum SeedError {
    #[error("mnemonic must be 12 or 24 words")]
    BadWordCount,
    #[error("randomness unavailable")]
    Rng,
    #[error("invalid mnemonic")]
    Mnemonic(#[from] bip39::Error),
    #[error("key derivation failed")]
    Bip32(#[from] bdk_wallet::bitcoin::bip32::Error),
    #[error("invalid descriptor")]
    Descriptor(#[from] bdk_wallet::descriptor::DescriptorError),
}

#[derive(Debug, Error)]
pub enum SignerError {
    #[error("signing failed")]
    Sign(#[from] bdk_wallet::signer::SignerError),
    #[error("signer produced an unfinalized transaction")]
    Unfinalized,
    #[error("cannot load signing key")]
    Seed(#[from] SeedError),
}

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("cannot build transaction")]
    CreateTx(#[from] Box<bdk_wallet::error::CreateTxError>),
    #[error("plan fee unknown")]
    Fee(#[from] bdk_wallet::bitcoin::psbt::Error),
    #[error("invalid psbt: {0}")]
    Psbt(String),
    #[error("cannot extract transaction: {0}")]
    Extract(String),
}
