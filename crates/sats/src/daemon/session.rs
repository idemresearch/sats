//! The daemon's key state: locked, or holding the master seed in memory.
//!
//! Locked is a first-class state, not an error condition. A locked daemon
//! still runs, still answers `Status`, and refuses every signature with a
//! typed `wallet_locked` code — an agent must be able to tell "your budget
//! said no" from "no human has unlocked the wallet".
//!
//! The seed is held as a phrase in a zeroize-on-drop buffer and parsed
//! into a `Mnemonic` only for the moment a signature is produced, keeping
//! derived key material in the shortest practical scope.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bdk_wallet::bip39::Mnemonic;
use bdk_wallet::descriptor::ExtendedDescriptor;
use bdk_wallet::{KeychainKind, Wallet};
use sats_core::bitcoin::Network;
use sats_core::{seal, seed};
use zeroize::Zeroizing;

use crate::store::{AAD_SEED, Store};

/// The unsealed wallet: the phrase, plus the two public descriptors the
/// PSBT verifier needs. Descriptors are derived once at unlock so every
/// authorization does not re-derive them.
struct Unlocked {
    phrase: Zeroizing<String>,
    external: ExtendedDescriptor,
    internal: ExtendedDescriptor,
    /// Last activity, for the idle auto-lock.
    touched: Instant,
}

pub struct Session {
    pub network: Network,
    pub net_name: &'static str,
    auto_lock_after: Duration,
    key: Mutex<Option<Unlocked>>,
}

/// The descriptors and phrase needed to verify and sign one transaction.
/// Cloned out of the session so the lock is not held across signing.
pub struct SigningKey {
    phrase: Zeroizing<String>,
    pub external: ExtendedDescriptor,
    pub internal: ExtendedDescriptor,
    pub network: Network,
}

impl SigningKey {
    /// Parse the mnemonic for one signature. The caller drops it as soon
    /// as the PSBT is finalized.
    pub fn mnemonic(&self) -> Result<Mnemonic> {
        Ok(seed::parse_mnemonic(&self.phrase)?)
    }
}

impl Session {
    pub fn new(network: Network, net_name: &'static str, auto_lock_after: Duration) -> Session {
        Session {
            network,
            net_name,
            auto_lock_after,
            key: Mutex::new(None),
        }
    }

    pub fn is_locked(&self) -> bool {
        self.key.lock().is_ok_and(|key| key.is_none())
    }

    /// Seconds until the idle auto-lock fires, or `None` while locked.
    pub fn locks_in(&self) -> Option<u64> {
        let key = self.key.lock().ok()?;
        let unlocked = key.as_ref()?;
        Some(
            self.auto_lock_after
                .saturating_sub(unlocked.touched.elapsed())
                .as_secs(),
        )
    }

    /// Unseal the master seed with the human's password.
    ///
    /// Deriving the descriptors here also proves the phrase is usable for
    /// this network before the daemon reports itself unlocked.
    pub fn unlock(&self, store: &Store, password: &str) -> Result<()> {
        let blob = store.read_seed()?;
        let bytes = Zeroizing::new(seal::open(&blob, password.as_bytes(), AAD_SEED)?);
        let phrase = Zeroizing::new(
            std::str::from_utf8(&bytes)
                .context("corrupt seed")?
                .to_string(),
        );
        // Deriving the descriptors here also proves the phrase is usable
        // before the daemon reports itself unlocked. `Mnemonic` is not
        // zeroize-on-drop, which is why only `phrase` is kept and a
        // mnemonic is re-parsed for each signature.
        let (external, internal) = descriptors(&seed::parse_mnemonic(&phrase)?, self.network)?;

        let mut key = self.key.lock().map_err(|_| poisoned())?;
        *key = Some(Unlocked {
            phrase,
            external,
            internal,
            touched: Instant::now(),
        });
        Ok(())
    }

    /// Drop the seed. Idempotent, so a `Lock` on an already-locked daemon
    /// is a success rather than an error.
    pub fn lock(&self) -> Result<()> {
        let mut key = self.key.lock().map_err(|_| poisoned())?;
        *key = None;
        Ok(())
    }

    /// Take a signing handle and mark the session active. Fails while
    /// locked so callers cannot accidentally treat it as an empty grant.
    pub fn signing_key(&self) -> Result<SigningKey> {
        let mut guard = self.key.lock().map_err(|_| poisoned())?;
        let Some(unlocked) = guard.as_mut() else {
            bail!("wallet is locked");
        };
        unlocked.touched = Instant::now();
        Ok(SigningKey {
            phrase: unlocked.phrase.clone(),
            external: unlocked.external.clone(),
            internal: unlocked.internal.clone(),
            network: self.network,
        })
    }

    /// Lock if nothing has used the key for the idle period. Returns
    /// whether this call was the one that locked it.
    pub fn lock_if_idle(&self) -> bool {
        let Ok(mut key) = self.key.lock() else {
            return false;
        };
        let idle = key
            .as_ref()
            .is_some_and(|unlocked| unlocked.touched.elapsed() >= self.auto_lock_after);
        if idle {
            *key = None;
        }
        idle
    }
}

/// The watch-only descriptors for a mnemonic, as parsed descriptor trees
/// rather than strings, so verification never re-parses per call.
fn descriptors(
    mnemonic: &Mnemonic,
    network: Network,
) -> Result<(ExtendedDescriptor, ExtendedDescriptor)> {
    let (ext, int) = seed::public_descriptors(mnemonic, network)?;
    let wallet = Wallet::create(ext, int)
        .network(network)
        .create_wallet_no_persist()
        .context("cannot derive wallet descriptors")?;
    Ok((
        wallet.public_descriptor(KeychainKind::External).clone(),
        wallet.public_descriptor(KeychainKind::Internal).clone(),
    ))
}

fn poisoned() -> anyhow::Error {
    anyhow::anyhow!("daemon key state is poisoned — restart satsd")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        let mnemonic = seed::parse_mnemonic(MNEMONIC).unwrap();
        let blob = seal::seal(mnemonic.to_string().as_bytes(), b"password", AAD_SEED).unwrap();
        store.write_seed(&blob).unwrap();
        (dir, store)
    }

    #[test]
    fn starts_locked_and_refuses_to_sign() {
        let session = Session::new(Network::Signet, "signet", Duration::from_secs(60));
        assert!(session.is_locked());
        assert!(session.locks_in().is_none());
        assert!(session.signing_key().is_err());
    }

    #[test]
    fn unlock_then_lock_round_trips() {
        let (_dir, store) = store();
        let session = Session::new(Network::Signet, "signet", Duration::from_secs(60));

        assert!(session.unlock(&store, "wrong").is_err(), "wrong password");
        assert!(session.is_locked(), "a failed unlock leaves it locked");

        session.unlock(&store, "password").unwrap();
        assert!(!session.is_locked());
        assert!(session.locks_in().unwrap() <= 60);

        let key = session.signing_key().unwrap();
        assert_eq!(key.mnemonic().unwrap().to_string(), MNEMONIC);

        session.lock().unwrap();
        assert!(session.is_locked());
        assert!(session.signing_key().is_err());
        // Locking twice is not an error.
        session.lock().unwrap();
    }

    #[test]
    fn idle_auto_lock_fires_once() {
        let (_dir, store) = store();
        let session = Session::new(Network::Signet, "signet", Duration::from_secs(0));
        session.unlock(&store, "password").unwrap();
        assert!(!session.is_locked());

        assert!(session.lock_if_idle(), "a zero idle window locks at once");
        assert!(session.is_locked());
        assert!(
            !session.lock_if_idle(),
            "an already-locked session does not re-report"
        );
    }

    #[test]
    fn a_long_idle_window_does_not_lock() {
        let (_dir, store) = store();
        let session = Session::new(Network::Signet, "signet", Duration::from_secs(3600));
        session.unlock(&store, "password").unwrap();
        assert!(!session.lock_if_idle());
        assert!(!session.is_locked());
    }

    #[test]
    fn descriptors_match_the_watch_only_wallet() {
        let mnemonic = seed::parse_mnemonic(MNEMONIC).unwrap();
        let (ext, int) = descriptors(&mnemonic, Network::Signet).unwrap();
        let (ext_str, int_str) = seed::public_descriptors(&mnemonic, Network::Signet).unwrap();
        assert_eq!(ext.to_string(), ext_str);
        assert_eq!(int.to_string(), int_str);
        assert!(!ext.to_string().contains("prv"));
    }
}
