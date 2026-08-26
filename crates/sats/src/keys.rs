//! Unsealing the master seed with the wallet password.
//!
//! There is exactly one way to turn a password into signing material, and
//! it is here. Agent grants carry no key material at all: they name a
//! policy that `satsd` enforces against a seed it unsealed itself.

use anyhow::{Context, Result};
use bdk_wallet::bip39::Mnemonic;
use sats_core::{seal, seed};

use crate::password;
use crate::store::{AAD_SEED, Store};

/// Prompt for (or read) the password and unseal the mnemonic.
pub fn unlock(store: &Store) -> Result<Mnemonic> {
    let blob = store.read_seed()?;
    let pw = password::get(false)?;
    let bytes = seal::open(&blob, pw.as_bytes(), AAD_SEED)?;
    let phrase = std::str::from_utf8(&bytes).context("corrupt seed")?;
    Ok(seed::parse_mnemonic(phrase)?)
}

/// Prove the human knows the wallet password, without keeping anything.
///
/// Used where the password is the authorization gesture rather than a
/// source of key material — issuing a grant, approving one request.
pub fn verify_password(store: &Store) -> Result<()> {
    let _ = unlock(store)?;
    Ok(())
}
