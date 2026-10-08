//! Protected vault, end to end: install the debug binary setuid to a
//! throwaway system account, then drive it as an unprivileged caller.
//! Needs root to create the accounts, so it's ignored by default; CI runs it
//! with sudo: `cargo test -p sats --test vault --no-run`, then run the test
//! binary as root with `--ignored`.
#![cfg(target_os = "linux")]

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const VAULT_USER: &str = "_satsvaulttest";
const CALLER: &str = "satsvaultcaller";
const PASSWORD: &str = "vault-test-password";

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        // SAFETY: geteuid has no preconditions.
        assert_eq!(unsafe { libc::geteuid() }, 0, "run this test as root");
        ensure_user(
            VAULT_USER,
            &[
                "--system",
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
            ],
        );
        ensure_user(CALLER, &["--create-home"]);

        // Not TMPDIR: the caller must be able to traverse every path component.
        let root = PathBuf::from(format!("/tmp/sats-vault-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("vault")).unwrap();
        for dir in [&root, &root.join("bin")] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let vault = root.join("vault");
        run_ok(
            Command::new("chown")
                .arg(format!("{VAULT_USER}:"))
                .arg(&vault),
        );
        std::fs::set_permissions(&vault, std::fs::Permissions::from_mode(0o700)).unwrap();

        let bin = root.join("bin/sats");
        std::fs::copy(env!("CARGO_BIN_EXE_sats"), &bin).unwrap();
        run_ok(
            Command::new("chown")
                .arg(format!("{VAULT_USER}:"))
                .arg(&bin),
        );
        // After chown, which clears set-user-ID.
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o4555)).unwrap();
        Fixture { root }
    }

    fn vault(&self) -> PathBuf {
        self.root.join("vault")
    }

    /// Run sats as the caller, with a clean environment plus `env`.
    fn sats(&self, args: &[&str], env: &[(&str, &str)], stdin: Option<&str>) -> Output {
        let mut cmd = Command::new("runuser");
        cmd.args([
            "-u",
            CALLER,
            "--",
            "env",
            "-i",
            "PATH=/usr/bin:/bin",
            "NO_COLOR=1",
        ]);
        cmd.arg(format!("SATS_VAULT_ROOT={}", self.vault().display()));
        cmd.arg(format!("SATS_PASSWORD={PASSWORD}"));
        for (k, v) in env {
            cmd.arg(format!("{k}={v}"));
        }
        cmd.arg(self.root.join("bin/sats")).args(args);
        cmd.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        if let Some(input) = stdin {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }
        child.wait_with_output().unwrap()
    }

    /// Run a plain command as the caller.
    fn as_caller(&self, program: &str, args: &[&str]) -> Output {
        Command::new("runuser")
            .args(["-u", CALLER, "--", program])
            .args(args)
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn ensure_user(name: &str, flags: &[&str]) {
    let exists = Command::new("id")
        .arg(name)
        .output()
        .unwrap()
        .status
        .success();
    if !exists {
        run_ok(Command::new("useradd").args(flags).arg(name));
    }
}

fn run_ok(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn uid_of(name: &str) -> u32 {
    let out = Command::new("id").args(["-u", name]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[test]
#[ignore = "needs root: creates system accounts and a setuid binary"]
fn the_vault_keeps_wallet_state_out_of_the_callers_reach() {
    let fx = Fixture::new();
    let caller_dir = fx.vault().join("users").join(uid_of(CALLER).to_string());
    let ignored_dir = fx.root.join("sats-dir-must-stay-unused");

    // A wallet lands in the caller's vault directory, not in SATS_DIR.
    let out = fx.sats(
        &["init", "--words", "12"],
        &[("SATS_DIR", ignored_dir.to_str().unwrap())],
        None,
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        !ignored_dir.exists(),
        "SATS_DIR must be ignored in the vault"
    );

    // Owned by the vault account and owner-only, whatever the caller's umask.
    let seed = caller_dir.join("seed.sealed");
    let vault_uid = uid_of(VAULT_USER);
    assert_eq!(std::fs::metadata(&seed).unwrap().uid(), vault_uid);
    assert_eq!(mode(&seed), 0o600);
    assert_eq!(mode(&fx.vault().join("users")), 0o700);
    assert_eq!(mode(&caller_dir), 0o700);

    // The caller can't read the vault directly...
    let out = fx.as_caller("cat", &[seed.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(text(&out).contains("Permission denied"), "{}", text(&out));

    // ...nor through sats: caller-named paths are opened as the caller.
    let out = fx.sats(&["psbt", "inspect", seed.to_str().unwrap()], &[], None);
    assert!(!out.status.success());
    assert!(text(&out).contains("Permission denied"), "{}", text(&out));

    // The caller's own owner-only file is still readable through sats.
    let own = PathBuf::from(format!("/home/{CALLER}/own.psbt"));
    std::fs::write(&own, "not a psbt").unwrap();
    run_ok(Command::new("chown").arg(format!("{CALLER}:")).arg(&own));
    std::fs::set_permissions(&own, std::fs::Permissions::from_mode(0o600)).unwrap();
    let out = fx.sats(&["psbt", "inspect", own.to_str().unwrap()], &[], None);
    assert!(text(&out).contains("not a valid PSBT"), "{}", text(&out));

    // A caller-chosen directory or file:// provider can't redirect the vault.
    let out = fx.sats(&["--dir", "/tmp", "balance", "--offline"], &[], None);
    assert!(text(&out).contains("--dir is not available with the protected vault"));
    let out = fx.sats(
        &[
            "--provider",
            &format!("esplora=file://{}", caller_dir.display()),
            "balance",
        ],
        &[],
        None,
    );
    assert!(
        text(&out).contains("file:// providers are not available"),
        "{}",
        text(&out)
    );

    // Grants print a launch command with nothing to pin, and the MCP server
    // serves through the vault.
    let out = fx.sats(
        &["agent", "grant", "bot", "--budget", "10k", "--for", "1h"],
        &[],
        None,
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("-- sats --network signet agent serve bot"),
        "{}",
        text(&out)
    );

    let out = fx.sats(
        &[
            "--json", "agent", "grant", "bot", "--budget", "10k", "--for", "1h",
        ],
        &[],
        None,
    );
    let grant: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let token = grant["token"].as_str().unwrap().to_string();
    let session = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"vault-test","version":"0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_grant","arguments":{}}}"#,
    ]
    .join("\n")
        + "\n";
    let out = fx.sats(
        &["agent", "serve", "bot"],
        &[("SATS_AGENT_TOKEN", &token)],
        Some(&session),
    );
    let last = String::from_utf8_lossy(&out.stdout)
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap()
        .to_string();
    let reply: serde_json::Value = serde_json::from_str(&last).unwrap();
    let grant = &reply["result"]["structuredContent"];
    assert_eq!(grant["agent"], "bot", "{reply}");
    assert_eq!(grant["active"], true, "{reply}");
}
