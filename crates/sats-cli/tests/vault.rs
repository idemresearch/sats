//! Run as root in Linux CI. Installs the debug binary setuid to a disposable
//! account, then exercises the real filesystem/identity boundary as another user.
#![cfg(all(target_os = "linux", feature = "mcp"))]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

const PASSWORD: &str = "vault-test-password";

struct Fixture {
    root: tempfile::TempDir,
    vault_user: String,
    caller: String,
    vault_created: bool,
    caller_created: bool,
}

impl Fixture {
    fn new() -> Self {
        // SAFETY: geteuid has no preconditions.
        assert_eq!(unsafe { libc::geteuid() }, 0, "run this test as root");
        let mut fx = Self {
            root: tempfile::Builder::new()
                .prefix("sats-vault-test-")
                .tempdir_in("/tmp")
                .unwrap(),
            vault_user: format!("_satsvault{}", std::process::id()),
            caller: format!("satscaller{}", std::process::id()),
            vault_created: false,
            caller_created: false,
        };
        chmod(fx.root.path(), 0o755);
        for name in [&fx.vault_user, &fx.caller] {
            assert!(
                !Command::new("id")
                    .arg(name)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "test account already exists"
            );
        }
        run_ok(Command::new("useradd").args([
            "--system",
            "--user-group",
            "--no-create-home",
            "--shell",
            "/usr/sbin/nologin",
            &fx.vault_user,
        ]));
        fx.vault_created = true;
        run_ok(
            Command::new("useradd")
                .args(["--system", "--user-group", "--no-create-home", "--home-dir"])
                .arg(fx.home())
                .arg(&fx.caller),
        );
        fx.caller_created = true;
        for dir in [fx.root.path().join("bin"), fx.vault(), fx.home()] {
            std::fs::create_dir(&dir).unwrap();
            chmod(&dir, 0o755);
        }
        chown(&fx.vault(), &fx.vault_user);
        chmod(&fx.vault(), 0o700);
        chown(&fx.home(), &fx.caller);
        chmod(&fx.home(), 0o700);
        std::fs::copy(env!("CARGO_BIN_EXE_sats"), fx.bin()).unwrap();
        chown(&fx.bin(), &fx.vault_user);
        chmod(&fx.bin(), 0o4555); // chown clears setuid; mode goes last.
        fx
    }

    fn bin(&self) -> PathBuf {
        self.root.path().join("bin/sats")
    }
    fn vault(&self) -> PathBuf {
        self.root.path().join("vault")
    }
    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }
    fn wallet(&self) -> PathBuf {
        self.vault()
            .join("users")
            .join(uid_of(&self.caller).to_string())
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new("runuser");
        cmd.args([
            "-u",
            &self.caller,
            "--",
            "env",
            "-i",
            "PATH=/usr/bin:/bin",
            "NO_COLOR=1",
        ])
        .arg(format!("SATS_VAULT_ROOT={}", self.vault().display()))
        .arg(format!("SATS_PASSWORD={PASSWORD}"));
        for (k, v) in env {
            cmd.arg(format!("{k}={v}"));
        }
        cmd.arg(self.bin()).args(args).stdin(Stdio::null());
        cmd
    }

    fn sats(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.command(args, env).output().unwrap()
    }

    fn caller(&self, program: &str, args: &[&str]) -> Output {
        Command::new("runuser")
            .args(["-u", &self.caller, "--", program])
            .args(args)
            .output()
            .unwrap()
    }

    fn own_file(&self, path: &Path, data: &[u8]) {
        std::fs::write(path, data).unwrap();
        chown(path, &self.caller);
        chmod(path, 0o600);
    }

    fn mcp_handshake(&self, token: &str) {
        let mut child = self
            .command(&["agent", "serve", "bot"], &[("SATS_AGENT_TOKEN", token)])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (send, recv) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if send.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            writeln!(input, "{}", serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"vault-test","version":"0"}}})).unwrap();
            let reply: serde_json::Value =
                serde_json::from_str(&recv.recv_timeout(Duration::from_secs(15)).unwrap()).unwrap();
            assert_eq!(reply["id"], 1);
            writeln!(
                input,
                "{}",
                serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .unwrap();
            writeln!(input, "{}", serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_grant","arguments":{}}})).unwrap();
            let reply: serde_json::Value =
                serde_json::from_str(&recv.recv_timeout(Duration::from_secs(15)).unwrap()).unwrap();
            assert_eq!(
                reply["result"]["structuredContent"]["agent"], "bot",
                "{reply}"
            );
            assert_eq!(
                reply["result"]["structuredContent"]["active"], true,
                "{reply}"
            );
        }));
        drop(input);
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for (name, created) in [
            (&self.caller, self.caller_created),
            (&self.vault_user, self.vault_created),
        ] {
            if !created {
                continue;
            }
            let _ = Command::new("userdel").arg(name).output();
            let _ = Command::new("groupdel").arg(name).output();
        }
    }
}

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}
fn chown(path: &Path, name: &str) {
    run_ok(Command::new("chown").arg(format!("{name}:")).arg(path));
}
fn uid_of(name: &str) -> u32 {
    let out = Command::new("id").args(["-u", name]).output().unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}
fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().mode() & 0o7777
}
fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}
fn success(out: &Output) {
    assert!(out.status.success(), "{}", text(out));
}
fn refused(out: &Output, message: &str) {
    assert!(!out.status.success(), "unexpected success: {}", text(out));
    assert!(text(out).contains(message), "{}", text(out));
}
fn run_ok(cmd: &mut Command) {
    success(&cmd.output().unwrap());
}

/// Prepare a real PSBT using the initialized watch-only descriptors. Funding is
/// confined to this in-memory test wallet: no mock provider is enabled in sats.
fn unsigned_psbt(dir: &Path) -> String {
    use bdk_wallet::bitcoin::hashes::Hash;
    use bdk_wallet::bitcoin::{Amount, BlockHash, FeeRate, Network};
    use bdk_wallet::chain::{BlockId, ConfirmationBlockTime};
    use bdk_wallet::test_utils::{insert_checkpoint, receive_output};
    let mut conn = rusqlite::Connection::open(dir.join("signet/wallet.sqlite")).unwrap();
    let mut wallet = bdk_wallet::Wallet::load()
        .check_network(Network::Signet)
        .load_wallet(&mut conn)
        .unwrap()
        .unwrap();
    let block = BlockId {
        height: 900,
        hash: BlockHash::all_zeros(),
    };
    insert_checkpoint(&mut wallet, block);
    insert_checkpoint(
        &mut wallet,
        BlockId {
            height: 1000,
            hash: BlockHash::all_zeros(),
        },
    );
    receive_output(
        &mut wallet,
        Amount::from_sat(100_000),
        ConfirmationBlockTime {
            block_id: block,
            confirmation_time: 100,
        },
    );
    let recipient = wallet
        .reveal_next_address(bdk_wallet::KeychainKind::External)
        .address;
    sats_core::engine::build_plan(
        &mut wallet,
        &recipient,
        Amount::from_sat(25_000),
        FeeRate::from_sat_per_vb_u32(2),
        &[],
        "signet",
        0,
    )
    .unwrap()
    .psbt()
    .to_string()
}

#[test]
#[ignore = "needs root: creates disposable accounts and a setuid executable"]
fn protected_vault_is_the_only_release_layout() {
    let fx = Fixture::new();
    // The installer-created vault works before a seed or configuration exists.
    success(&fx.sats(&["providers", "list"], &[]));
    assert!(!fx.wallet().join("seed.sealed").exists());

    // Unsafe/missing roots and symlinks fail closed, before opening a wallet.
    chmod(&fx.vault(), 0o755);
    refused(&fx.sats(&["providers"], &[]), "run the installer");
    chmod(&fx.vault(), 0o700);
    let link = fx.root.path().join("vault-link");
    std::os::unix::fs::symlink(fx.vault(), &link).unwrap();
    refused(
        &fx.sats(
            &["providers"],
            &[("SATS_VAULT_ROOT", link.to_str().unwrap())],
        ),
        "run the installer",
    );
    refused(
        &fx.sats(
            &["providers"],
            &[("SATS_VAULT_ROOT", "/no-such-sats-test-vault")],
        ),
        "run the installer",
    );

    let ignored_dir = fx.root.path().join("ignored");
    // A caller-owned legacy wallet is detected through passwd, not HOME. It
    // remains intact until the human has verified recovery and rotated funds.
    let legacy = fx.home().join(".local/share/sats");
    std::fs::create_dir_all(&legacy).unwrap();
    fx.own_file(&legacy.join("seed.sealed"), b"legacy seed sentinel");
    success(&fx.sats(
        &["init", "--words", "12"],
        &[
            ("SATS_DIR", ignored_dir.to_str().unwrap()),
            ("HOME", "/wrong-home"),
        ],
    ));
    assert!(!ignored_dir.exists());
    let notice = fx.sats(&["init"], &[]);
    assert!(text(&notice).contains("legacy wallet"));
    assert!(text(&notice).contains("treat that seed") || text(&notice).contains("Treat that seed"));
    assert_eq!(
        std::fs::read(legacy.join("seed.sealed")).unwrap(),
        b"legacy seed sentinel"
    );

    let seed = fx.wallet().join("seed.sealed");
    assert_eq!(
        std::fs::metadata(&seed).unwrap().uid(),
        uid_of(&fx.vault_user)
    );
    assert_eq!(mode(&seed), 0o600);
    assert_eq!(mode(&fx.vault().join("users")), 0o700);
    assert_eq!(mode(&fx.wallet()), 0o700);
    refused(
        &fx.caller("cat", &[seed.to_str().unwrap()]),
        "Permission denied",
    );
    refused(
        &fx.caller("ls", &[fx.vault().to_str().unwrap()]),
        "Permission denied",
    );
    refused(
        &fx.sats(&["psbt", "inspect", seed.to_str().unwrap()], &[]),
        "Permission denied",
    );
    refused(
        &fx.sats(&["psbt", "sign", seed.to_str().unwrap()], &[]),
        "Permission denied",
    );
    refused(
        &fx.sats(&["tx", "broadcast", seed.to_str().unwrap()], &[]),
        "Permission denied",
    );
    refused(
        &fx.sats(&["--dir", "/tmp", "balance", "--offline"], &[]),
        "--dir is not available",
    );
    for driver in ["esplora", "subfrost"] {
        refused(
            &fx.sats(
                &[
                    "--provider",
                    &format!("{driver}=file://{}", fx.wallet().display()),
                    "balance",
                ],
                &[],
            ),
            "file:// providers are not available",
        );
    }

    // All exported artifacts are caller-owned, and a failed caller I/O scope
    // does not leave the next wallet operation running as the caller.
    let own = fx.home().join("own.psbt");
    fx.own_file(&own, unsigned_psbt(&fx.wallet()).as_bytes());
    success(&fx.sats(&["psbt", "inspect", own.to_str().unwrap()], &[]));
    let exported = fx.home().join("exported.psbt");
    success(&fx.sats(
        &[
            "psbt",
            "sign",
            own.to_str().unwrap(),
            "--out",
            exported.to_str().unwrap(),
        ],
        &[],
    ));
    assert_eq!(
        std::fs::metadata(&exported).unwrap().uid(),
        uid_of(&fx.caller)
    );
    assert_eq!(mode(&exported), 0o600);
    refused(
        &fx.sats(
            &[
                "psbt",
                "sign",
                own.to_str().unwrap(),
                "--out",
                fx.wallet().join("escape.psbt").to_str().unwrap(),
            ],
            &[],
        ),
        "Permission denied",
    );
    success(&fx.sats(&["balance", "--offline"], &[]));

    let out = fx.sats(&["agent", "grant", "bot", "--budget", "10k"], &[]);
    success(&out);
    assert!(
        text(&out).contains(&format!(
            "-- {} --network signet agent serve bot",
            fx.bin().display()
        )),
        "{}",
        text(&out)
    );
    assert!(!text(&out).contains("--dir"));
    let out = fx.sats(&["--json", "agent", "grant", "bot", "--budget", "10k"], &[]);
    success(&out);
    let grant: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    fx.mcp_handshake(grant["token"].as_str().unwrap());

    // sudo is never a wallet entry point, but installer metadata is exempt.
    refused(
        &Command::new(fx.bin()).arg("balance").output().unwrap(),
        "run sats as yourself, not with sudo",
    );
    success(&Command::new(fx.bin()).arg("--version").output().unwrap());
    success(&Command::new(fx.bin()).arg("--help").output().unwrap());
}
