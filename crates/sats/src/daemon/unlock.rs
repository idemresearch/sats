//! Human consent stays on the daemon side of the socket. The MCP caller
//! can request a prompt, never supply a password or choose its contents.

use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::session::{Session, UnlockError};
use crate::store::{Store, now_checked};

#[cfg(target_os = "macos")]
mod macos;

const COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UnlockStatus {
    Unlocked,
    Cancelled,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UnlockCode {
    DaemonUnavailable,
    Unauthorized,
    ClockUnavailable,
    UnsupportedPlatform,
    UnlockInProgress,
    UnlockRateLimited,
    UnlockFailed,
    UnlockThrottled,
    UnlockTimeout,
    PromptUnavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
pub struct UnlockResult {
    pub status: UnlockStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<UnlockCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    pub message: String,
}

impl UnlockResult {
    pub fn error(code: UnlockCode, message: &str) -> Self {
        Self {
            status: UnlockStatus::Error,
            error_code: Some(code),
            retry_after_seconds: None,
            message: message.into(),
        }
    }

    pub fn cancelled() -> Self {
        Self {
            status: UnlockStatus::Cancelled,
            error_code: None,
            retry_after_seconds: None,
            message: "Unlock cancelled. Do not prompt again unless the human asks.".into(),
        }
    }

    fn unlocked() -> Self {
        Self {
            status: UnlockStatus::Unlocked,
            error_code: None,
            retry_after_seconds: None,
            message: "Daemon unlocked. All active grants for this wallet/network can now request signing within their existing limits. No payment was authorized or sent by this call.".into(),
        }
    }

    fn retry(mut self, after: Duration) -> Self {
        self.retry_after_seconds = Some(after.as_secs().saturating_add(1));
        self
    }
}

/// Held across the prompt, without holding the seed or grant mutexes.
/// Other status, lock, and send connections remain responsive.
#[derive(Default)]
pub(super) struct PromptGate(Mutex<Option<Instant>>);

struct Permit<'a>(MutexGuard<'a, Option<Instant>>);

impl PromptGate {
    fn enter(&self, now: Instant) -> Result<Permit<'_>, UnlockResult> {
        let last = match self.0.try_lock() {
            Ok(last) => last,
            Err(TryLockError::WouldBlock) => {
                return Err(UnlockResult::error(
                    UnlockCode::UnlockInProgress,
                    "An unlock dialog is already open for this daemon. Do not open another.",
                ));
            }
            Err(TryLockError::Poisoned(_)) => return Err(prompt_unavailable()),
        };
        if let Some(wait) = last.and_then(|at| (at + COOLDOWN).checked_duration_since(now)) {
            return Err(UnlockResult::error(
                UnlockCode::UnlockRateLimited,
                "Unlock dialogs are rate limited. Wait, and only retry at the human's request.",
            )
            .retry(wait));
        }
        Ok(Permit(last))
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        *self.0 = Some(Instant::now());
    }
}

fn authenticate(
    store: &Store,
    session: &Session,
    agent: &str,
    token: &str,
) -> Result<(), UnlockResult> {
    let unauthorized = || {
        UnlockResult::error(
            UnlockCode::Unauthorized,
            "An active matching grant and token are required to request an unlock. Ask the human to check the grant; do not request their password in chat.",
        )
    };
    let grant = store
        .load_grant(session.net_name, agent)
        .map_err(|_| unauthorized())?
        .ok_or_else(unauthorized)?;
    let now = now_checked().map_err(|_| {
        UnlockResult::error(
            UnlockCode::ClockUnavailable,
            "Cannot verify grant expiry because the system clock is unavailable.",
        )
    })?;
    if grant.agent != agent || !grant.authorizes(token) || grant.is_expired(now) {
        return Err(unauthorized());
    }
    Ok(())
}

pub(super) fn request(
    store: &Store,
    session: &Session,
    agent: &str,
    token: &str,
    connected: impl FnMut() -> bool,
) -> UnlockResult {
    request_with(store, session, agent, token, connected, native_prompt)
}

fn request_with(
    store: &Store,
    session: &Session,
    agent: &str,
    token: &str,
    mut connected: impl FnMut() -> bool,
    prompt: impl FnOnce(&str, &mut dyn FnMut() -> bool) -> Result<Zeroizing<String>, UnlockResult>,
) -> UnlockResult {
    if let Err(result) = authenticate(store, session, agent, token) {
        return result;
    }
    let generation = session.generation();
    if !session.is_locked() {
        return UnlockResult::unlocked();
    }
    let _permit = match session.prompts.enter(Instant::now()) {
        Ok(permit) => permit,
        Err(result) => return result,
    };
    let mut consent = || session.generation() == generation && connected();
    if !consent() {
        return UnlockResult::cancelled();
    }
    // Debug-format the canonical path to escape control characters, quotes
    // and newlines. Neither the agent nor tool arguments supply dialog text.
    let wallet = match store.wallet_db_path(session.net_name).canonicalize() {
        Ok(path) => path,
        Err(_) => return prompt_unavailable(),
    };
    let message = format!(
        "Agent: {agent}\nNetwork: {}\nWallet: {:?}\nIdle auto-lock: {}\n\nUnlocking enables ALL active grants for this daemon within their existing limits, not just one payment.\n\nEnter your sats wallet password here only. Never paste it into chat.",
        session.net_name,
        wallet,
        humantime::format_duration(session.auto_lock_after())
    );
    let password = match prompt(&message, &mut consent) {
        Ok(password) => password,
        Err(result) => return result,
    };
    // Re-read under the same grant lock used by revoke/replace. Do not keep
    // that lock while the dialog waits on its human.
    let _grants = match store.lock_grants(session.net_name) {
        Ok(lock) => lock,
        Err(_) => return prompt_unavailable(),
    };
    if let Err(result) = authenticate(store, session, agent, token) {
        return result;
    }
    match session.unlock_if(store, &password, || {
        consent() && authenticate(store, session, agent, token).is_ok()
    }) {
        Ok(()) => UnlockResult::unlocked(),
        Err(UnlockError::Cancelled) => UnlockResult::cancelled(),
        Err(UnlockError::Throttled { retry_in }) => UnlockResult::error(
            UnlockCode::UnlockThrottled,
            "Too many failed passwords. Wait before trying again.",
        )
        .retry(retry_in),
        // Never forward helper output, passwords, or decryption details.
        Err(UnlockError::Failed(_)) => UnlockResult::error(
            UnlockCode::UnlockFailed,
            "Wallet could not be unlocked. Check the password in the local dialog, or use sats daemon unlock in your terminal.",
        ),
    }
}

fn prompt_unavailable() -> UnlockResult {
    UnlockResult::error(
        UnlockCode::PromptUnavailable,
        "Cannot open the local unlock dialog. Use sats daemon unlock in a terminal in the wallet's logged-in desktop session.",
    )
}

fn native_prompt(
    message: &str,
    consent: &mut dyn FnMut() -> bool,
) -> Result<Zeroizing<String>, UnlockResult> {
    #[cfg(target_os = "macos")]
    {
        macos::prompt(message, consent)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (message, consent);
        Err(UnlockResult::error(
            UnlockCode::UnsupportedPlatform,
            "Local password dialogs are supported only on macOS. Run sats daemon unlock in your terminal.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sats_core::{
        authz::{GRANT_FORMAT_VERSION, Grant},
        bitcoin::Network,
        seal, token,
    };

    const PASSWORD: &str = "test-only-password";
    const MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn fixture() -> (tempfile::TempDir, Store, Zeroizing<String>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        store
            .write_seed(
                &seal::seal(
                    MNEMONIC.as_bytes(),
                    PASSWORD.as_bytes(),
                    crate::store::AAD_SEED,
                )
                .unwrap(),
            )
            .unwrap();
        store
            .create_private_dirs(store.wallet_db_path("signet").parent().unwrap())
            .unwrap();
        std::fs::write(store.wallet_db_path("signet"), b"path identity only").unwrap();
        let token = token::generate().unwrap();
        store
            .save_grant(
                "signet",
                &Grant {
                    format_version: GRANT_FORMAT_VERSION,
                    agent: "claude".into(),
                    network: "signet".into(),
                    budget_sat: 50000,
                    spent_sat: 123,
                    max_tx_sat: Some(1000),
                    max_fee_sat: 200,
                    created_at: 0,
                    expires_at: u64::MAX,
                    tx_count: 1,
                    token_hash: token.token_hash,
                    token_id: token.token_id,
                    mode: Default::default(),
                    allowed_recipients: None,
                },
            )
            .unwrap();
        (dir, store, token.secret)
    }

    fn session() -> Session {
        Session::new(Network::Signet, "signet", Duration::from_secs(3600))
    }

    #[test]
    fn unlock_requires_consent_and_preserves_grants_and_accounting() {
        let (_dir, store, token) = fixture();
        let session = session();
        let before = std::fs::read(store.grants_dir("signet").join("claude.json")).unwrap();
        let result = request_with(
            &store,
            &session,
            "claude",
            &token,
            || true,
            |message, consent| {
                assert!(message.contains("Agent: claude"));
                assert!(message.contains("Network: signet"));
                assert!(message.contains("Idle auto-lock: 1h"));
                assert!(message.contains("ALL active grants"));
                assert!(message.contains("wallet.sqlite"));
                assert!(!message.contains(&*token));
                assert!(consent());
                Ok(Zeroizing::new(PASSWORD.into()))
            },
        );
        assert_eq!(result.status, UnlockStatus::Unlocked);
        assert!(!session.is_locked());
        assert!(session.locks_in().unwrap() <= 3600);
        assert_eq!(
            before,
            std::fs::read(store.grants_dir("signet").join("claude.json")).unwrap()
        );
        assert!(!store.transactions_dir("signet").exists());
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains(PASSWORD));
        assert!(!serialized.contains(&*token));
        assert!(!serialized.contains(MNEMONIC));
        let already = request_with(
            &store,
            &session,
            "claude",
            &token,
            || true,
            |_, _| panic!("already unlocked"),
        );
        assert_eq!(already.status, UnlockStatus::Unlocked);
    }

    #[test]
    fn invalid_authentication_never_opens_a_prompt_even_if_unlocked() {
        let (_dir, store, token) = fixture();
        let session = session();
        for agent in ["claude", "missing", "../claude"] {
            let result = request_with(
                &store,
                &session,
                agent,
                "wrong",
                || true,
                |_, _| panic!("unauthenticated UI"),
            );
            assert_eq!(result.error_code, Some(UnlockCode::Unauthorized));
        }
        session.unlock(&store, PASSWORD).unwrap();
        let mut grant = store.load_grant("signet", "claude").unwrap().unwrap();
        grant.expires_at = 1;
        store.save_grant("signet", &grant).unwrap();
        let expired = request_with(
            &store,
            &session,
            "claude",
            &token,
            || true,
            |_, _| panic!("expired UI"),
        );
        assert_eq!(expired.error_code, Some(UnlockCode::Unauthorized));
    }

    #[test]
    fn cancelled_wrong_password_and_disconnected_requests_leave_wallet_locked() {
        let (_dir, store, token) = fixture();
        for mode in [
            "cancel",
            "wrong",
            "disconnect",
            "lock",
            "unavailable",
            "timeout",
        ] {
            let session = session();
            let connected = std::cell::Cell::new(true);
            let result = request_with(
                &store,
                &session,
                "claude",
                &token,
                || connected.get(),
                |_, _| {
                    match mode {
                        "cancel" => return Err(UnlockResult::cancelled()),
                        "unavailable" => return Err(prompt_unavailable()),
                        "timeout" => {
                            return Err(UnlockResult::error(UnlockCode::UnlockTimeout, "timeout"));
                        }
                        "disconnect" => connected.set(false),
                        "lock" => session.lock().unwrap(),
                        _ => {}
                    }
                    Ok(Zeroizing::new(
                        if mode == "wrong" { "wrong" } else { PASSWORD }.into(),
                    ))
                },
            );
            assert!(session.is_locked(), "{mode}");
            if mode == "wrong" {
                assert_eq!(result.error_code, Some(UnlockCode::UnlockFailed));
            } else if ["cancel", "disconnect", "lock"].contains(&mode) {
                assert_eq!(result.status, UnlockStatus::Cancelled);
            }
            let retry = request_with(
                &store,
                &session,
                "claude",
                &token,
                || true,
                |_, _| panic!("cooldown"),
            );
            assert_eq!(retry.error_code, Some(UnlockCode::UnlockRateLimited));
        }
    }

    #[test]
    fn grant_revocation_rotation_expiry_and_network_change_during_prompt_refuse_unlock() {
        let (_dir, store, token) = fixture();
        let original = store.load_grant("signet", "claude").unwrap().unwrap();
        for mode in ["revoke", "rotate", "expire", "network", "agent"] {
            store.save_grant("signet", &original).unwrap();
            let session = session();
            let result = request_with(
                &store,
                &session,
                "claude",
                &token,
                || true,
                |_, _| {
                    let mut grant = original.clone();
                    match mode {
                        "revoke" => {
                            store.delete_grant("signet", "claude").unwrap();
                        }
                        "rotate" => {
                            grant.token_hash = token::generate().unwrap().token_hash;
                            store.save_grant("signet", &grant).unwrap();
                        }
                        "expire" => {
                            grant.expires_at = 1;
                            store.save_grant("signet", &grant).unwrap();
                        }
                        _ => {
                            if mode == "network" {
                                grant.network = "bitcoin".into();
                            } else {
                                grant.agent = "other".into();
                            }
                            std::fs::write(
                                store.grants_dir("signet").join("claude.json"),
                                serde_json::to_vec(&grant).unwrap(),
                            )
                            .unwrap();
                        }
                    }
                    Ok(Zeroizing::new(PASSWORD.into()))
                },
            );
            assert_eq!(result.error_code, Some(UnlockCode::Unauthorized), "{mode}");
            assert!(session.is_locked());
        }
    }

    #[test]
    fn only_one_prompt_runs_and_the_cooldown_expires() {
        let gate = PromptGate::default();
        let permit = gate.enter(Instant::now()).ok().unwrap();
        assert_eq!(
            gate.enter(Instant::now()).err().unwrap().error_code,
            Some(UnlockCode::UnlockInProgress)
        );
        drop(permit);
        let last = gate.0.lock().unwrap().unwrap();
        assert_eq!(
            gate.enter(last).err().unwrap().error_code,
            Some(UnlockCode::UnlockRateLimited)
        );
        assert!(
            gate.enter(last + COOLDOWN + Duration::from_millis(1))
                .is_ok()
        );
    }

    #[test]
    fn later_human_lock_wins_at_the_commit_boundary() {
        let (_dir, store, _) = fixture();
        let session = session();
        let generation = session.generation();
        session.lock().unwrap();
        assert!(matches!(
            session.unlock_if(&store, PASSWORD, || session.generation() == generation),
            Err(UnlockError::Cancelled)
        ));
        assert!(session.is_locked());
    }
}
