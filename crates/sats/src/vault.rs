//! Protected vault: sats installed setuid to a dedicated, unprivileged
//! account (`_sats`) that alone owns the wallet directory. The caller, and
//! any agent running as the caller, can run sats but can't read or edit its
//! state, copy the sealed seed, or read the process's memory (the kernel
//! marks setuid processes non-dumpable).
//!
//! A process is protected when its effective uid differs from its real uid.
//! It then:
//! - clears its environment to an allowlist before anything reads it;
//! - keeps one vault directory per real uid, which a caller can't forge;
//! - opens files the caller names (PSBTs, raw transactions) as the caller,
//!   so a path can never reach into the vault.
//!
//! The process otherwise runs as the vault account. Any new code that
//! touches a caller-named path must go through [`as_caller`]. An ordinary
//! install (no setuid bit) behaves exactly as before.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[cfg(target_os = "macos")]
const VAULT_ROOT: &str = "/var/db/sats";
#[cfg(not(target_os = "macos"))]
const VAULT_ROOT: &str = "/var/lib/sats";

/// Environment kept in protected mode. Everything else is caller-controlled
/// input to a process running as the vault account, so it is dropped.
const KEEP_ENV: &[&str] = &[
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    // The agent's own credential for `agent serve`; it authenticates the
    // caller and grants nothing by itself.
    "SATS_AGENT_TOKEN",
];

/// Kept only in debug builds (see `password::TEST_SEAM`): the test password
/// and a vault root the setuid integration test can point at a temp dir.
const TEST_ENV: &[&str] = &["SATS_PASSWORD", "SATS_VAULT_ROOT"];

struct Protected {
    caller: libc::uid_t,
    vault: libc::uid_t,
    dir: PathBuf,
}

static MODE: OnceLock<Option<Protected>> = OnceLock::new();

/// Detect protected mode and, if active, sanitize the environment. Must run
/// first in `main`: before any thread starts and before anything reads the
/// environment.
pub fn enter() {
    // SAFETY: getuid/geteuid have no preconditions and cannot fail.
    let (caller, vault) = unsafe { (libc::getuid(), libc::geteuid()) };
    let mode = (caller != vault).then(|| {
        let root = test_root().unwrap_or_else(|| PathBuf::from(VAULT_ROOT));
        sanitize_env();
        // Everything the vault account creates is owner-only, whatever the
        // caller's umask; files would otherwise inherit the caller's group.
        // SAFETY: umask only sets this process's file-creation mask.
        unsafe { libc::umask(0o077) };
        Protected {
            caller,
            vault,
            dir: root.join("users").join(caller.to_string()),
        }
    });
    let _ = MODE.set(mode);
}

/// Whether this process runs as the vault account on the caller's behalf.
pub fn is_protected() -> bool {
    mode().is_some()
}

/// The caller's wallet directory inside the vault, in protected mode.
pub fn caller_dir() -> Option<&'static Path> {
    mode().map(|p| p.dir.as_path())
}

/// Run `f` with the caller's own file permissions: for paths the caller
/// names on the command line. Outside protected mode, just runs `f`.
pub fn as_caller<T>(f: impl FnOnce() -> T) -> T {
    let Some(p) = mode() else { return f() };
    switch_to(p.caller);
    let out = f();
    switch_to(p.vault);
    out
}

fn mode() -> Option<&'static Protected> {
    MODE.get().and_then(Option::as_ref)
}

fn switch_to(uid: libc::uid_t) {
    // SAFETY: seteuid only changes this process's effective uid. Between
    // the real and saved set-user-ID it never needs privilege. A failure
    // would leave the process with the wrong identity, so stop instead.
    if unsafe { libc::seteuid(uid) } != 0 {
        eprintln!(
            "✗ cannot switch identity: {}",
            std::io::Error::last_os_error()
        );
        std::process::abort();
    }
}

fn sanitize_env() {
    let keep = |name: &str| {
        KEEP_ENV.contains(&name) || (crate::password::TEST_SEAM && TEST_ENV.contains(&name))
    };
    let drop: Vec<_> = std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| !name.to_str().is_some_and(keep))
        .collect();
    for name in drop {
        // SAFETY: called from `enter`, first thing in `main`, before any
        // other thread exists.
        unsafe { std::env::remove_var(name) };
    }
}

fn test_root() -> Option<PathBuf> {
    if !crate::password::TEST_SEAM {
        return None;
    }
    std::env::var_os("SATS_VAULT_ROOT").map(PathBuf::from)
}
