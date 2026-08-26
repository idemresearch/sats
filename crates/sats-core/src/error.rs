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
    #[error("invalid transaction record: {0}")]
    Transaction(String),
}

#[derive(Debug, Error)]
pub enum TokenError {
    #[error("randomness unavailable")]
    Rng,
    #[error("malformed agent token")]
    Malformed,
}

#[derive(Debug, Error)]
pub enum VerifyError {
    #[error("psbt has no inputs")]
    NoInputs,
    #[error("malformed psbt: {0}")]
    Malformed(String),
    #[error("input {0} has no witness_utxo, so its value is unknown")]
    MissingWitnessUtxo(usize),
    #[error("input {0} does not belong to this wallet")]
    ForeignInput(usize),
    #[error("output {0} is not a decodable address on this network")]
    UndecodableOutput(usize),
    #[error("outputs are worth more than the inputs")]
    OutputsExceedInputs,
    #[error("transaction values overflow")]
    ValueOverflow,
    #[error("transaction pays no one")]
    NoRecipient,
    #[error("transaction pays {0} recipients; sats sends pay exactly one")]
    MultipleRecipients(usize),
}
