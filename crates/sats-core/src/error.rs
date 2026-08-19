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
    #[error("invalid mnemonic: {0}")]
    Mnemonic(#[from] bip39::Error),
    #[error("key derivation failed: {0}")]
    Bip32(#[from] bdk_wallet::bitcoin::bip32::Error),
    #[error("descriptor error: {0}")]
    Descriptor(#[from] bdk_wallet::descriptor::DescriptorError),
}

#[derive(Debug, Error)]
pub enum SignerError {
    #[error("signing failed: {0}")]
    Sign(#[from] bdk_wallet::signer::SignerError),
    #[error("signer produced an unfinalized transaction")]
    Unfinalized,
    #[error("{0}")]
    Seed(#[from] SeedError),
}

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("cannot build transaction: {0}")]
    CreateTx(#[from] Box<bdk_wallet::error::CreateTxError>),
    #[error("plan fee unknown: {0}")]
    Fee(#[from] bdk_wallet::bitcoin::psbt::Error),
    #[error("malformed plan: {0}")]
    Malformed(String),
}
