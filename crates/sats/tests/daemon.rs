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

    assert_eq!(grant["format_version"], 1);
    assert_eq!(grant["mode"], "ask", "every fresh grant asks by default");
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

/// A pre-daemon wrapped-seed grant is read well enough to name itself,
/// then refused. Honoring one would preserve exactly the weakness the
/// daemon removes — the one pre-release shape with a dedicated message,
/// because the right advice is seed rotation, not a re-grant.
#[test]
fn a_wrapped_seed_grant_is_refused_with_its_migration_path() {
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
        .stdout(predicate::str::contains("pre-daemon"))
        .stdout(predicate::str::contains("sats agent revoke claude"));

    // The JSON contract stays an array; the notice goes to stderr.
    let out = sats(&dir)
        .args(["--json", "agent", "list"])
        .assert()
        .success();
    let listed: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert!(listed.is_array(), "list --json must stay an array");
    assert_eq!(listed.as_array().unwrap().len(), 0);
    assert!(String::from_utf8_lossy(&out.get_output().stderr).contains("pre-daemon"));

    // And nothing will act on it.
    sats(&dir)
        .args(["agent", "serve", "claude"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("pre-daemon"))
        .stderr(predicate::str::contains("treat the seed as disclosed"));
}

/// A grant authorizes only its own network. Copying a signet grant file
/// into another network's directory must not let it authorize signing
/// there — the confinement the removed v1 seal AAD used to guarantee.
#[test]
fn a_grant_is_confined_to_its_network() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    // A real signet grant, with its genuine token hash.
    grant_json(&dir, "claude");
    let signet_grant = std::fs::read(dir.path().join("signet/grants/claude.json")).unwrap();

    // Drop that exact file into mainnet's grant directory.
    let mainnet_grants = dir.path().join("mainnet/grants");
    std::fs::create_dir_all(&mainnet_grants).unwrap();
    std::fs::write(mainnet_grants.join("claude.json"), &signet_grant).unwrap();

    // Serving as that agent on mainnet must refuse before anything else:
    // the grant on disk names signet, not mainnet.
    sats(&dir)
        .args(["--network", "mainnet", "agent", "serve", "claude"])
        .env("SATS_AGENT_TOKEN", "unused")
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not match mainnet"));
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

/// The socket is reachable by anything running as the wallet's user, so
/// repeated wrong passwords must stop being tried: after the free misses
/// the daemon refuses further attempts for a while, with a typed code.
#[test]
fn repeated_wrong_passwords_hit_the_throttle() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let _daemon = Daemon::start(&dir);

    let attempt = |password: &str| -> String {
        let mut cmd = assert_cmd::Command::cargo_bin("sats").unwrap();
        let output = cmd
            .args(["daemon", "unlock"])
            .env("SATS_DIR", dir.path())
            .env("SATS_PASSWORD", password)
            .env("NO_COLOR", "1")
            .assert()
            .failure();
        String::from_utf8_lossy(&output.get_output().stderr).into_owned()
    };

    // Three misses are free, the fourth arms the block.
    for _ in 0..4 {
        let failed = attempt("not-the-password");
        assert!(failed.contains("unlock_failed"), "got: {failed}");
    }
    // The fifth is refused without being tried — even the right password.
    let throttled = attempt(PASSWORD);
    assert!(
        throttled.contains("too many failed unlock attempts"),
        "got: {throttled}"
    );

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

#[test]
fn daemon_lock_is_acquired_before_stale_socket_cleanup() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join("signet")).unwrap();
    let socket = dir.path().join("signet/d.sock");
    let lock = std::fs::File::create(socket.with_extension("lock")).unwrap();
    lock.lock().unwrap();
    let stale = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    drop(stale);
    sats(&dir)
        .args(["daemon", "run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already running or starting"));
    assert!(
        socket.exists(),
        "a competing process must not unlink the socket"
    );
    drop(lock);
    let _daemon = Daemon::start(&dir);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(socket.with_extension("lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

/// Explicitly opt in: this touches launchd and ~/Library/LaunchAgents, but only
/// for a disposable wallet. It never starts or stops a user's wallet service.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires a macOS GUI session and permission to register an isolated launchd service"]
fn managed_service_lifecycle() {
    let dir = tempfile::Builder::new()
        .prefix("sats-service-")
        .tempdir_in("/tmp")
        .unwrap();
    let binary = std::env::var("SATS_TEST_BINARY").unwrap_or_else(|_| sats_bin().into());
    let call = |args: &[&str]| {
        Command::new(&binary)
            .env("SATS_DIR", dir.path())
            .env("SATS_PASSWORD", PASSWORD)
            .args(args)
            .output()
            .unwrap()
    };
    struct Cleanup<'a> {
        binary: &'a str,
        dir: &'a TempDir,
    }
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = Command::new(self.binary)
                .env("SATS_DIR", self.dir.path())
                .args(["daemon", "uninstall"])
                .output();
        }
    }
    let _cleanup = Cleanup {
        binary: &binary,
        dir: &dir,
    };
    assert!(call(&["init"]).status.success());
    let issued = call(&["--json", "agent", "grant", "smoke", "--budget", "1000"]);
    assert!(issued.status.success());
    let grant: serde_json::Value = serde_json::from_slice(&issued.stdout).unwrap();
    std::fs::write(dir.path().join("preserve-me"), "wallet metadata").unwrap();
    let mut unmanaged = Daemon(
        Command::new(&binary)
            .env("SATS_DIR", dir.path())
            .args(["daemon", "run"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert!((0..100).any(|_| {
        if call(&["daemon", "status"]).status.success() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
        false
    }));
    let refused = call(&["daemon", "install"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("unmanaged"));
    assert!(call(&["daemon", "stop"]).status.success());
    unmanaged.0.wait().unwrap();
    drop(unmanaged);
    let installed = call(&["--json", "daemon", "install", "--auto-lock", "30m"]);
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let install: serde_json::Value = serde_json::from_slice(&installed.stdout).unwrap();
    let plist = std::path::Path::new(install["plist"].as_str().unwrap());
    let original = std::fs::read(plist).unwrap();
    assert!(!String::from_utf8_lossy(&original).contains(PASSWORD));
    assert!(!String::from_utf8_lossy(&original).contains(grant["token"].as_str().unwrap()));
    assert!(
        call(&["daemon", "install", "--auto-lock", "30m"])
            .status
            .success()
    );
    assert_eq!(std::fs::read(plist).unwrap(), original);
    // The installer process is already gone; launchd owns this child.
    let status: serde_json::Value =
        serde_json::from_slice(&call(&["--json", "daemon", "status"]).stdout).unwrap();
    assert_eq!(status["locked"], true);
    // Launch and close two MCP adapters while the same managed daemon remains.
    #[cfg(feature = "mcp")]
    for _ in 0..2 {
        use std::io::{BufRead, Write};
        let mut child = Command::new(&binary)
            .env("SATS_DIR", dir.path())
            .env("SATS_AGENT_TOKEN", grant["token"].as_str().unwrap())
            .args(["agent", "serve", "smoke"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
        writeln!(input, "{}", serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"service-smoke","version":"1"}}})).unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        let initialized: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(initialized.get("result").is_some());
        writeln!(
            input,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
        writeln!(
            input,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"get_status","arguments":{}}})
        )
        .unwrap();
        line.clear();
        output.read_line(&mut line).unwrap();
        let status: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            status["result"]["structuredContent"]["daemon_state"],
            "locked"
        );
        drop(input);
        child.wait().unwrap();
        assert!(call(&["daemon", "status"]).status.success());
    }
    assert!(
        !call(&["daemon", "install", "--auto-lock", "1h"])
            .status
            .success()
    );
    assert!(
        !call(&["daemon", "start", "--auto-lock", "8h"])
            .status
            .success()
    );
    assert!(call(&["daemon", "stop"]).status.success());
    std::thread::sleep(Duration::from_secs(2));
    let status: serde_json::Value =
        serde_json::from_slice(&call(&["--json", "daemon", "status"]).stdout).unwrap();
    assert_eq!(
        status["running"], false,
        "explicit stop must not auto-restart"
    );
    assert!(call(&["daemon", "start"]).status.success());
    assert!(call(&["daemon", "unlock"]).status.success());
    let unlocked: serde_json::Value =
        serde_json::from_slice(&call(&["--json", "daemon", "status"]).stdout).unwrap();
    assert_eq!(unlocked["locked"], false);
    let label = install["service"].as_str().unwrap();
    let uid = Command::new("/usr/bin/id").arg("-u").output().unwrap();
    let target = format!(
        "gui/{}/{}",
        String::from_utf8(uid.stdout).unwrap().trim(),
        label
    );
    let service_pid = || {
        let output = Command::new("/bin/launchctl")
            .args(["print", &target])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.trim().strip_prefix("pid = ").map(str::to_string))
    };
    let before = service_pid().unwrap();
    assert!(
        Command::new("/bin/launchctl")
            .args(["kill", "SIGKILL", &target])
            .status()
            .unwrap()
            .success()
    );
    let restarted = (0..200).any(|_| {
        std::thread::sleep(Duration::from_millis(100));
        service_pid().is_some_and(|pid| pid != before)
            && call(&["daemon", "status"]).status.success()
    });
    assert!(restarted, "launchd must restart an unexpected failure");
    let status: serde_json::Value =
        serde_json::from_slice(&call(&["--json", "daemon", "status"]).stdout).unwrap();
    assert_eq!(status["locked"], true);
    assert!(call(&["daemon", "stop"]).status.success());
    assert!(
        call(&["daemon", "install", "--auto-lock", "1h"])
            .status
            .success()
    );
    assert!(call(&["daemon", "uninstall"]).status.success());
    assert!(!plist.exists());
    assert!(dir.path().join("preserve-me").exists());
    assert!(dir.path().join("seed.sealed").exists());
    assert!(dir.path().join("signet/grants/smoke.json").exists());
    assert!(dir.path().join("signet/satsd.log").exists());
    assert!(call(&["daemon", "uninstall"]).status.success());
}
