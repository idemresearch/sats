//! Shared transaction-composition facts for the Alkanes builders: the
//! default postage and the error every builder reports.

use crate::protostone::EncodeError;

/// Default sats carried by the pointer output. 546 is deliberate: the
/// wallet's own dust heuristic then protects the resulting asset-bearing
/// UTXO from later coin selection.
pub const DEFAULT_POSTAGE_SAT: u64 = 546;

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error(transparent)]
    Encode(#[from] EncodeError),
    #[error("cannot create transaction: {0}")]
    CreateTx(String),
    #[error("cannot compute fee: {0}")]
    Fee(String),
}
