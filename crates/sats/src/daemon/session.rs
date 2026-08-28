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
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Escalating delay after failed unlock attempts.
///
/// Pure over an injected `Instant` so the schedule is testable without
/// sleeping. The socket is reachable by any process running as the
/// wallet's user, which makes an ungated unlock a password-guessing
/// oracle; the first misses are free (humans mistype), then the delay
/// doubles per miss up to a cap. Monotonic time, so a wall-clock
/// rollback cannot lift a block early.
struct UnlockThrottle {
    failures: u32,
    blocked_until: Option<Instant>,
    base: Duration,
}

/// Failed attempts before any delay applies.
const FREE_FAILURES: u32 = 3;
/// Ceiling on the per-attempt delay.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

impl UnlockThrottle {
    fn new(base: Duration) -> UnlockThrottle {
        UnlockThrottle {
            failures: 0,
            blocked_until: None,
            base,
        }
    }

    /// Whether an attempt may run now; `Err` carries the remaining wait.
    fn check(&self, now: Instant) -> Result<(), Duration> {
        match self.blocked_until {
            Some(until) if now < until => Err(until - now),
            _ => Ok(()),
        }
    }

    fn record_failure(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        if self.failures > FREE_FAILURES {
            let doublings = (self.failures - FREE_FAILURES - 1).min(31);
            let delay = self.base.saturating_mul(1u32 << doublings).min(MAX_BACKOFF);
            self.blocked_until = Some(now + delay);
        }
    }

    fn record_success(&mut self) {
        self.failures = 0;
        self.blocked_until = None;
    }
}

pub struct Session {
    pub network: Network,
    pub net_name: &'static str,
    auto_lock_after: Duration,
    key: Mutex<Option<Unlocked>>,
    unlock_gate: Mutex<UnlockThrottle>,
    generation: AtomicU64,
    pub(super) prompts: super::unlock::PromptGate,
}

/// Why an unlock attempt did not unlock.
#[derive(Debug)]
pub enum UnlockError {
    Cancelled,
    /// Refused without touching the sealed seed: too many recent misses.
    Throttled {
        retry_in: Duration,
    },
    /// The attempt ran and failed (wrong password, unreadable seed, …).
    Failed(anyhow::Error),
}

impl std::fmt::Display for UnlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnlockError::Cancelled => write!(f, "unlock request cancelled"),
            UnlockError::Throttled { retry_in } => write!(
                f,
                "too many failed unlock attempts — retry in {}s",
                retry_in.as_secs().max(1)
            ),
            UnlockError::Failed(err) => write!(f, "{err:#}"),
        }
    }
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
            unlock_gate: Mutex::new(UnlockThrottle::new(Duration::from_secs(1))),
            generation: AtomicU64::new(0),
            prompts: super::unlock::PromptGate::default(),
        }
    }

    /// Test constructor: a session whose unlock backoff starts at `base`.
    #[cfg(test)]
    fn with_unlock_base(self, base: Duration) -> Session {
        Session {
            unlock_gate: Mutex::new(UnlockThrottle::new(base)),
            ..self
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
    /// The throttle gate is held across the whole attempt: concurrent
    /// unlocks queue on one mutex, so at most one Argon2id derivation
    /// (64 MiB) is in memory at a time, and repeated misses back off
    /// instead of turning the socket into a guessing oracle.
    pub fn unlock(&self, store: &Store, password: &str) -> Result<(), UnlockError> {
        self.unlock_if(store, password, || true)
    }

    /// Check consent again under the key mutex, after expensive derivation.
    /// A delayed dialog must never undo a subsequent human lock/unlock.
    pub(super) fn unlock_if(
        &self,
        store: &Store,
        password: &str,
        consent: impl FnOnce() -> bool,
    ) -> Result<(), UnlockError> {
        let mut gate = self
            .unlock_gate
            .lock()
            .map_err(|_| UnlockError::Failed(poisoned()))?;
        gate.check(Instant::now())
            .map_err(|retry_in| UnlockError::Throttled { retry_in })?;
        match self.try_unlock(store, password, consent) {
            Ok(true) => {
                gate.record_success();
                Ok(())
            }
            Ok(false) => Err(UnlockError::Cancelled),
            Err(err) => {
                gate.record_failure(Instant::now());
                Err(UnlockError::Failed(err))
            }
        }
    }

    /// One ungated unlock attempt. Deriving the descriptors here also
    /// proves the phrase is usable for this network before the daemon
    /// reports itself unlocked.
    fn try_unlock(
        &self,
        store: &Store,
        password: &str,
        consent: impl FnOnce() -> bool,
    ) -> Result<bool> {
        let blob = store.read_seed()?;
        let bytes = Zeroizing::new(seal::open(&blob, password.as_bytes(), AAD_SEED)?);
        let phrase = Zeroizing::new(
            std::str::from_utf8(&bytes)
                .context("corrupt seed")?
                .to_string(),
        );
        // `Mnemonic` is not zeroize-on-drop, which is why only `phrase` is
        // kept and a mnemonic is re-parsed for each signature.
        let (external, internal) = descriptors(&seed::parse_mnemonic(&phrase)?, self.network)?;

        let mut key = self.key.lock().map_err(|_| poisoned())?;
        if !consent() {
            return Ok(false);
        }
        self.generation.fetch_add(1, Ordering::SeqCst);
        *key = Some(Unlocked {
            phrase,
            external,
            internal,
            touched: Instant::now(),
        });
        Ok(true)
    }

    /// Drop the seed. Idempotent, so a `Lock` on an already-locked daemon
    /// is a success rather than an error.
    pub fn lock(&self) -> Result<()> {
        let mut key = self.key.lock().map_err(|_| poisoned())?;
        self.generation.fetch_add(1, Ordering::SeqCst);
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
            self.generation.fetch_add(1, Ordering::SeqCst);
            *key = None;
        }
        idle
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub(super) fn auto_lock_after(&self) -> Duration {
        self.auto_lock_after
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
    fn unlock_backoff_is_free_then_doubling_then_capped() {
        let t0 = Instant::now();
        let mut throttle = UnlockThrottle::new(Duration::from_secs(1));

        // Three misses cost nothing: humans mistype.
        for _ in 0..FREE_FAILURES {
            assert!(throttle.check(t0).is_ok());
            throttle.record_failure(t0);
        }
        assert!(throttle.check(t0).is_ok(), "free misses arm no delay");

        // The next misses double: 1s, 2s, 4s, … capped at 60s.
        let mut expected = Duration::from_secs(1);
        for _ in 0..8 {
            throttle.record_failure(t0);
            let retry_in = throttle.check(t0).unwrap_err();
            assert_eq!(retry_in, expected.min(MAX_BACKOFF));
            assert!(
                throttle.check(t0 + retry_in).is_ok(),
                "the block lifts exactly when it says"
            );
            expected *= 2;
        }
        // Far past the cap the delay stays capped.
        for _ in 0..40 {
            throttle.record_failure(t0);
        }
        assert_eq!(throttle.check(t0).unwrap_err(), MAX_BACKOFF);

        // Success resets everything.
        throttle.record_success();
        assert!(throttle.check(t0).is_ok());
        throttle.record_failure(t0);
        assert!(throttle.check(t0).is_ok(), "the free window re-arms");
    }

    #[test]
    fn a_throttled_unlock_refuses_even_the_correct_password() {
        let (_dir, store) = store();
        let session = Session::new(Network::Signet, "signet", Duration::from_secs(60))
            .with_unlock_base(Duration::from_secs(3600));

        // Three free misses, then the miss that arms the hour-long block.
        for _ in 0..4 {
            assert!(matches!(
                session.unlock(&store, "wrong"),
                Err(UnlockError::Failed(_))
            ));
        }
        // The block refuses before the attempt runs: the right password
        // is not even tried, so the wallet stays locked.
        assert!(matches!(
            session.unlock(&store, "password"),
            Err(UnlockError::Throttled { .. })
        ));
        assert!(session.is_locked());
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
