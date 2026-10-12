//! Process boundary for the protected wallet. Release commands run setuid
//! to the vault account; only explicit debug directories support development.
//! Caller-named file paths must be accessed through [`as_caller`].

use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};

use crate::cli::Cli;

#[cfg(target_os = "macos")]
const VAULT_ROOT: &str = "/var/db/sats";
#[cfg(not(target_os = "macos"))]
const VAULT_ROOT: &str = "/var/lib/sats";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Protected,
    Dev,
    Refuse,
}

/// Pure process-policy decision. Neither the caller nor the vault may be root.
pub fn decide(ruid: u32, euid: u32, debug: bool) -> Mode {
    if ruid == 0 || euid == 0 {
        Mode::Refuse
    } else if ruid != euid {
        Mode::Protected
    } else if debug {
        Mode::Dev
    } else {
        Mode::Refuse
    }
}

struct Runtime {
    mode: Mode,
    caller: libc::uid_t,
    vault: libc::uid_t,
    root: PathBuf,
}

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Must be the first call in main, before threads or environment readers.
/// Validation follows clap's help/version exit, so metadata remains available
/// to the installer even when the command itself would be refused.
pub fn enter() {
    // SAFETY: these identity queries have no preconditions.
    let (caller, vault) = unsafe { (libc::getuid(), libc::geteuid()) };
    let mode = decide(caller, vault, cfg!(debug_assertions));
    if caller != vault {
        sanitize_env();
    }
    if mode == Mode::Protected {
        // SAFETY: called before starting any other threads.
        unsafe { libc::umask(0o077) };
    }
    let root = PathBuf::from(VAULT_ROOT);
    #[cfg(debug_assertions)]
    let root = std::env::var_os("SATS_VAULT_ROOT")
        .map(PathBuf::from)
        .unwrap_or(root);
    assert!(
        RUNTIME
            .set(Runtime {
                mode,
                caller,
                vault,
                root
            })
            .is_ok(),
        "vault entered twice"
    );
}

pub fn is_protected() -> bool {
    RUNTIME.get().is_some_and(|r| r.mode == Mode::Protected)
}

/// A debug build outside the vault is the only file-provider test seam.
pub fn allows_file_providers() -> bool {
    cfg!(debug_assertions) && RUNTIME.get().is_some_and(|r| r.mode == Mode::Dev)
}

pub fn wallet_dir(cli: &Cli) -> Result<PathBuf> {
    let r = RUNTIME.get().context("vault was not initialized")?;
    match r.mode {
        Mode::Protected => {
            #[cfg(debug_assertions)]
            if cli.dir.is_some() {
                bail!("--dir is not available with the protected vault");
            }
            #[cfg(not(debug_assertions))]
            let _ = cli;
            validate_dir(&r.root, r.vault)?;
            let users = r.root.join("users");
            create_dir(&users, r.vault)?;
            let wallet = users.join(r.caller.to_string());
            create_dir(&wallet, r.vault)?;
            Ok(wallet)
        }
        Mode::Dev => {
            #[cfg(debug_assertions)]
            return cli.dir.clone().filter(|p| !p.as_os_str().is_empty())
                .context("debug builds require SATS_DIR or --dir pointing to an explicit development wallet");
            #[cfg(not(debug_assertions))]
            bail!("development wallets are not available in release builds");
        }
        Mode::Refuse if r.caller == 0 => bail!("run sats as yourself, not with sudo"),
        Mode::Refuse if r.vault == 0 => bail!("sats must not be setuid root — run the installer"),
        Mode::Refuse => bail!("install with `curl -fsSL https://sats.sh/setup.sh | sudo sh`"),
    }
}

fn validate_dir(path: &Path, owner: libc::uid_t) -> Result<()> {
    let meta = std::fs::symlink_metadata(path).with_context(|| {
        format!(
            "protected vault is unavailable at {} — run the installer",
            path.display()
        )
    })?;
    if !meta.is_dir() || meta.uid() != owner || meta.permissions().mode() & 0o7777 != 0o700 {
        bail!(
            "protected vault directory {} must be a real directory owned by the vault account with mode 0700 — run the installer",
            path.display()
        );
    }
    Ok(())
}

fn create_dir(path: &Path, owner: libc::uid_t) -> Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).context("cannot create wallet directory — run the installer"),
    }
    validate_dir(path, owner)
}

struct CallerGuard(libc::uid_t);

impl Drop for CallerGuard {
    fn drop(&mut self) {
        switch_to(self.0);
    }
}

/// Run only file I/O on caller-named paths with the caller's permissions.
/// This changes the identity of the WHOLE process, including provider threads,
/// and is not a thread-local sandbox. Do not overlap these scopes or run wallet,
/// provider, signing, or other privileged work in the closure. Never return an
/// open file for later privileged use. The guard restores the vault euid even
/// during unwinding; any identity-switch failure aborts immediately.
pub fn as_caller<T>(f: impl FnOnce() -> T) -> T {
    let Some(r) = RUNTIME.get().filter(|r| r.mode == Mode::Protected) else {
        return f();
    };
    switch_to(r.caller);
    let _guard = CallerGuard(r.vault);
    f()
}

fn switch_to(uid: libc::uid_t) {
    // SAFETY: only the real and saved set-user-ID are used. POSIX libc
    // coordinates the effective-uid change across the process's threads.
    if unsafe { libc::seteuid(uid) } != 0 {
        eprintln!(
            "✗ cannot switch identity: {}",
            std::io::Error::last_os_error()
        );
        std::process::abort();
    }
}

fn keep_env(name: &str, debug: bool) -> bool {
    matches!(
        name,
        "TERM" | "COLORTERM" | "NO_COLOR" | "LANG" | "SATS_AGENT_TOKEN"
    ) || name.starts_with("LC_")
        || (debug && matches!(name, "SATS_PASSWORD" | "SATS_VAULT_ROOT"))
}

fn sanitize_env() {
    let drop: Vec<_> = std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| {
            !name
                .to_str()
                .is_some_and(|n| keep_env(n, cfg!(debug_assertions)))
        })
        .collect();
    for name in drop {
        // SAFETY: enter is the first call in main, before any other threads.
        unsafe { std::env::remove_var(name) };
    }
}

/// Find only the previous platform default, using the account database instead
/// of caller-controlled HOME/XDG variables. No legacy data is removed or copied.
pub fn legacy_wallet() -> Result<Option<PathBuf>> {
    let Some(r) = RUNTIME.get().filter(|r| r.mode == Mode::Protected) else {
        return Ok(None);
    };
    as_caller(|| {
        let home = caller_home(r.caller)?;
        #[cfg(target_os = "macos")]
        let dir = home.join("Library/Application Support/sats");
        #[cfg(not(target_os = "macos"))]
        let dir = home.join(".local/share/sats");
        Ok(dir.join("seed.sealed").try_exists()?.then_some(dir))
    })
}

fn caller_home(uid: libc::uid_t) -> Result<PathBuf> {
    // getpwuid_r owns no shared static storage, unlike getpwuid; provider
    // threads may exist when a later init extension checks this notice.
    let mut size = 16384;
    loop {
        let mut buf = vec![0u8; size];
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: entry, buffer and result are valid writable allocations.
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && size < 1_048_576 {
            size *= 2;
            continue;
        }
        if rc != 0 || result.is_null() {
            bail!("cannot look up the caller's previous wallet directory");
        }
        // SAFETY: successful lookup initialized entry; pw_dir is a C string
        // inside buf, copied into the PathBuf before the buffer is dropped.
        let home = unsafe { CStr::from_ptr(entry.assume_init().pw_dir) };
        return Ok(PathBuf::from(OsStr::from_bytes(home.to_bytes())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_mode_is_fail_closed() {
        for debug in [false, true] {
            assert_eq!(decide(501, 499, debug), Mode::Protected);
            for (r, e) in [(0, 0), (0, 499), (501, 0)] {
                assert_eq!(decide(r, e, debug), Mode::Refuse);
            }
        }
        assert_eq!(decide(501, 501, true), Mode::Dev);
        assert_eq!(decide(501, 501, false), Mode::Refuse);
    }

    #[test]
    fn vault_environment_has_only_explicit_inputs() {
        for debug in [false, true] {
            for name in [
                "TERM",
                "COLORTERM",
                "NO_COLOR",
                "LANG",
                "LC_TIME",
                "SATS_AGENT_TOKEN",
            ] {
                assert!(keep_env(name, debug));
            }
            for name in [
                "HOME",
                "PATH",
                "SATS_DIR",
                "LD_PRELOAD",
                "DYLD_LIBRARY_PATH",
                "HTTPS_PROXY",
                "XDG_DATA_HOME",
            ] {
                assert!(!keep_env(name, debug));
            }
            for name in ["SATS_PASSWORD", "SATS_VAULT_ROOT"] {
                assert_eq!(keep_env(name, debug), debug);
            }
        }
    }

    #[test]
    fn vault_root_must_be_private_owned_and_real() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault");
        std::fs::create_dir(&path).unwrap();
        let uid = std::fs::metadata(&path).unwrap().uid();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        validate_dir(&path, uid).unwrap();
        assert!(validate_dir(&path, uid.wrapping_add(1)).is_err());
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(validate_dir(&link, uid).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(validate_dir(&path, uid).is_err());
        assert!(validate_dir(&dir.path().join("missing"), uid).is_err());
    }
}
