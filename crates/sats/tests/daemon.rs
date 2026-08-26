//! satsd integration tests: the signing boundary itself.
//!
//! These cover what the daemon exists for — that a grant file carries no
//! key material, that a v1 grant is refused rather than honored, and that
//! a locked daemon reports itself as locked rather than as a policy
//! denial. Fully offline; each test gets its own SATS_DIR and socket.

mod common;

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::{PASSWORD, init_wallet, sats};
use predicates::prelude::*;
use tempfile::TempDir;

fn sats_bin() -> &'static str {
    env!("CARGO_BIN_EXE_sats")
}

/// A backgrounded daemon, torn down with the test.
struct Daemon(Child);

impl Daemon {
    fn start(dir: &TempDir) -> Daemon {
        let child = Command::new(sats_bin())
            .args(["daemon", "run", "--auto-lock", "1h"])
            .env("SATS_DIR", dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let ready = (0..200).any(|_| {
            let up = Command::new(sats_bin())
                .args(["daemon", "status"])
                .env("SATS_DIR", dir.path())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !up {
                std::thread::sleep(Duration::from_millis(25));
            }
            up
        });
        assert!(ready, "satsd did not start");
        Daemon(child)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn grant_json(dir: &TempDir, agent: &str) -> serde_json::Value {
    let output = sats(dir)
        .args(["--json", "agent", "grant", agent, "--budget", "50000"])
        .assert()
        .success();
    serde_json::from_slice(&output.get_output().stdout).expect("json output")
}

/// The whole point: what lands on disk is a policy and a token hash, and
/// nothing that could ever produce a signature.
#[test]
fn a_grant_file_holds_no_key_material() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let issued = grant_json(&dir, "claude");
    let token = issued["token"].as_str().expect("token printed once");

    let raw = std::fs::read_to_string(dir.path().join("signet/grants/claude.json")).unwrap();
    let grant: serde_json::Value = serde_json::from_str(&raw).unwrap();

    assert_eq!(grant["format_version"], 2);
    assert!(grant.get("wrapped_seed").is_none(), "v1 field survived");
    assert!(grant.get("grant_key").is_none(), "v1 field survived");
    assert!(
        grant.get("token").is_none(),
        "the bearer token itself must never be persisted"
    );
    assert!(
        !raw.contains(token),
        "the grant file must not contain the token, only its hash"
    );
    assert_eq!(grant["token_id"], issued["token_id"]);
    assert_eq!(
        grant["token_hash"].as_str().unwrap().len(),
        64,
        "token_hash is a hex sha256"
    );
    // A reader of this file learns the budget, and nothing more useful.
    for word in ["mnemonic", "xprv", "seed", "abandon"] {
        assert!(!raw.contains(word), "grant file mentions {word:?}");
    }
}

/// Each grant mints a fresh token, and issuing a replacement kills the
/// old one — revocation and rotation are the same mechanism.
#[test]
fn re_granting_replaces_the_token() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let first = grant_json(&dir, "claude");
    let second = grant_json(&dir, "claude");

    assert_ne!(first["token"], second["token"]);
    assert_ne!(first["token_id"], second["token_id"]);
    assert_eq!(second["replaced"], true);

    let grant: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("signet/grants/claude.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(grant["token_id"], second["token_id"]);
}

/// A v1 grant is read well enough to name itself, then refused. Honoring
/// one would preserve exactly the weakness the daemon removes.
#[test]
fn a_v1_grant_is_refused_with_its_migration_path() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let grants = dir.path().join("signet/grants");
    std::fs::create_dir_all(&grants).unwrap();
    let legacy = serde_json::json!({
        "agent": "claude",
        "network": "signet",
        "budget_sat": 50_000,
        "spent_sat": 0,
        "max_tx_sat": null,
        "max_fee_sat": null,
        "created_at": 1_700_000_000u64,
        "expires_at": 4_000_000_000u64,
        "tx_count": 0,
        "wrapped_seed": { "version": 1, "kdf": "argon2id", "salt": "", "nonce": "", "ct": "" },
        "grant_key": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    });
    std::fs::write(
        grants.join("claude.json"),
        serde_json::to_vec_pretty(&legacy).unwrap(),
    )
    .unwrap();

    // Listing reports it rather than showing an empty, reassuring table.
    sats(&dir)
        .args(["agent", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("v1 format"))
        .stdout(predicate::str::contains("sats agent revoke claude"));

    // The JSON contract stays an array; the notice goes to stderr.
    let out = sats(&dir)
        .args(["--json", "agent", "list"])
        .assert()
        .success();
    let listed: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert!(listed.is_array(), "list --json must stay an array");
    assert_eq!(listed.as_array().unwrap().len(), 0);
    assert!(String::from_utf8_lossy(&out.get_output().stderr).contains("v1 format"));

    // And nothing will act on it.
    sats(&dir)
        .args(["agent", "serve", "claude"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("v1 format"))
        .stderr(predicate::str::contains("treat the seed as disclosed"));
}

/// Locked is a state the daemon serves from, not an error it dies of.
#[test]
fn the_daemon_locks_and_unlocks() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let _daemon = Daemon::start(&dir);

    let locked = |dir: &TempDir| -> serde_json::Value {
        let out = sats(dir)
            .args(["--json", "daemon", "status"])
            .assert()
            .success();
        serde_json::from_slice(&out.get_output().stdout).unwrap()
    };

    let status = locked(&dir);
    assert_eq!(status["running"], true);
    assert_eq!(status["locked"], true, "a fresh daemon holds no seed");

    sats(&dir).args(["daemon", "unlock"]).assert().success();
    let status = locked(&dir);
    assert_eq!(status["locked"], false);
    assert!(status["locks_in"].as_u64().unwrap() <= 3600);

    sats(&dir).args(["daemon", "lock"]).assert().success();
    assert_eq!(locked(&dir)["locked"], true);

    // Locking twice is not an error: it is already what was asked for.
    sats(&dir).args(["daemon", "lock"]).assert().success();
}

/// A wrong password leaves the daemon locked rather than half-unlocked.
#[test]
fn a_wrong_password_does_not_unlock() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let _daemon = Daemon::start(&dir);

    let mut wrong = assert_cmd::Command::cargo_bin("sats").unwrap();
    wrong
        .args(["daemon", "unlock"])
        .env("SATS_DIR", dir.path())
        .env("SATS_PASSWORD", "not-the-password")
        .env("NO_COLOR", "1")
        .assert()
        .failure();

    let out = sats(&dir)
        .args(["--json", "daemon", "status"])
        .assert()
        .success();
    let status: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(status["locked"], true);
}

/// The socket is the access control, so its permissions are the test.
#[test]
fn the_socket_is_owner_only() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let _daemon = Daemon::start(&dir);

    let socket = dir.path().join("signet/d.sock");
    assert!(socket.exists(), "expected a socket at {}", socket.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket must not be readable by other users");
    }
}

/// Every command that needs the daemon says how to start it.
#[test]
fn commands_needing_the_daemon_name_the_remedy() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);

    for args in [
        vec!["daemon", "status"],
        vec!["daemon", "unlock"],
        vec!["daemon", "lock"],
        vec!["daemon", "stop"],
    ] {
        sats(&dir).args(&args).assert().failure().stderr(
            predicate::str::contains("sats daemon start")
                .or(predicate::str::contains("satsd is not running")),
        );
    }

    // `status --json` is for scripts: it reports rather than fails.
    let out = sats(&dir)
        .args(["--json", "daemon", "status"])
        .assert()
        .success();
    let status: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(status["running"], false);
}

/// Stop is graceful: it answers, then exits and clears its socket.
#[test]
fn stop_removes_the_socket() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let daemon = Daemon::start(&dir);
    let socket = dir.path().join("signet/d.sock");
    assert!(socket.exists());

    sats(&dir).args(["daemon", "stop"]).assert().success();
    drop(daemon);

    let gone = (0..100).any(|_| {
        if socket.exists() {
            std::thread::sleep(Duration::from_millis(20));
            false
        } else {
            true
        }
    });
    assert!(gone, "stop must clear its socket");
}

/// A second daemon on the same network is refused, not silently racing
/// the first for the same grant state.
#[test]
fn two_daemons_cannot_share_a_network() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let _daemon = Daemon::start(&dir);

    sats(&dir)
        .args(["daemon", "start"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already running"));

    let mut second = assert_cmd::Command::cargo_bin("sats").unwrap();
    second
        .args(["daemon", "run"])
        .env("SATS_DIR", dir.path())
        .env("SATS_PASSWORD", PASSWORD)
        .env("NO_COLOR", "1")
        .assert()
        .failure()
        .stderr(predicate::str::contains("already running"));
}
