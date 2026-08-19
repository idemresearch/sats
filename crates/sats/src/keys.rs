//! Unsealing the master seed with the wallet password.

use anyhow::{Context, Result};
use bdk_wallet::bip39::Mnemonic;
use sats_core::authz::Grant;
use sats_core::{seal, seed};

use crate::password;
use crate::store::{AAD_SEED, Store, grant_aad};

/// Prompt for (or read) the password and unseal the mnemonic.
pub fn unlock(store: &Store) -> Result<Mnemonic> {
    let blob = store.read_seed()?;
    let pw = password::get(false)?;
    let bytes = seal::open(&blob, pw.as_bytes(), AAD_SEED)?;
    let phrase = std::str::from_utf8(&bytes).context("corrupt seed")?;
    Ok(seed::parse_mnemonic(phrase)?)
}

/// Unseal the grant-wrapped mnemonic — the unattended signing path.
/// Only callable while the grant file (key + wrapped seed) exists.
pub fn unlock_grant(grant: &Grant, network: &str) -> Result<Mnemonic> {
    let key = seal::decode_key_b64(&grant.grant_key)?;
    let bytes = seal::open_with_key(&grant.wrapped_seed, &key, &grant_aad(network, &grant.agent))?;
    let phrase = std::str::from_utf8(&bytes).context("corrupt grant")?;
    Ok(seed::parse_mnemonic(phrase)?)
}
