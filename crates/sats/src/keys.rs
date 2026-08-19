//! Unsealing the master seed with the wallet password.

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
