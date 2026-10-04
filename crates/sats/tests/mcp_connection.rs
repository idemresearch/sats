//! Execute both printed POSIX connection commands without client installations.
//! The fixture records exact arguments/environment, then launches the real MCP
//! server against disposable wallets, with hostile ambient defaults and cwd.
#![cfg(all(unix, feature = "mcp"))]

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

const AGENT: &str = "connection-agent";

struct Fixture {
    root: TempDir,
    home: PathBuf,
    config: PathBuf,
    data: PathBuf,
    cwd: PathBuf,
    other: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let home = root.path().join("home ' $HOME ; & `literal`");
        let config = root.path().join("config ' $(literal) &");
        let data = root.path().join("data ' $USER ;");
        let cwd = root.path().join("creation");
        let other = root.path().join("connection");
        let bin = root.path().join("bin");
        for dir in [&home, &config, &data, &cwd, &other, &bin] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(other.join("config.toml"), "network = \"mainnet\"\n").unwrap();
        for (name, script) in [
            (
                "codex",
                r#"#!/bin/sh
set -eu
printf '%s\0' "$@" > "$SATS_FIXTURE_CODEX_ARGS"
test "$1" = mcp && test "$2" = add && test "$3" = sats
shift 3
test "$1" = --env
shift
export "$1"
shift
test "$1" = --
shift
case "${SATS_FIXTURE_TOKEN_MODE-}" in
    missing) unset SATS_AGENT_TOKEN ;;
    wrong) export SATS_AGENT_TOKEN=wrong-token ;;
esac
exec "$@"
"#,
            ),
            (
                "claude",
                r#"#!/bin/sh
set -eu
printf '%s\0' "$@" > "$SATS_FIXTURE_CLAUDE_ARGS"
test "$1" = mcp && test "$2" = add
shift 2
test "$1" = --transport && test "$2" = stdio
shift 2
test "$1" = --scope && test "$2" = local
shift 2
test "$1" = sats
shift
test "$1" = --env
shift
export "$1"
shift
test "$1" = --
shift
case "${SATS_FIXTURE_TOKEN_MODE-}" in
    missing) unset SATS_AGENT_TOKEN ;;
    wrong) export SATS_AGENT_TOKEN=wrong-token ;;
esac
exec "$@"
"#,
            ),
            (
                "sats",
                r#"#!/bin/sh
set -eu
printf '%s\0' "$@" > "$SATS_FIXTURE_SATS_ARGS"
printf '%s\0' "${SATS_DIR-unset}" "$HOME" "${XDG_CONFIG_HOME-unset}" "${XDG_DATA_HOME-unset}" "${SATS_AGENT_TOKEN-}" > "$SATS_FIXTURE_ENV"
exec "$SATS_FIXTURE_BINARY" "$@"
"#,
            ),
        ] {
            let path = bin.join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self {
            root,
            home,
            config,
            data,
            cwd,
            other,
            bin,
        }
    }

    fn sats(&self, dir: Option<&Path>, network: Option<&str>, xdg: bool) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sats"));
        cmd.current_dir(&self.cwd)
            .env_remove("SATS_DIR")
            .env("HOME", &self.home)
            .env("SATS_PASSWORD", common::PASSWORD)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null());
        if xdg {
            cmd.env("XDG_CONFIG_HOME", &self.config)
                .env("XDG_DATA_HOME", &self.data);
        } else {
            cmd.env_remove("XDG_CONFIG_HOME")
                .env_remove("XDG_DATA_HOME");
        }
        if let Some(dir) = dir {
            if dir.is_relative() {
                cmd.env("SATS_DIR", dir);
            } else {
                cmd.arg("--dir").arg(dir);
            }
        }
        if let Some(network) = network {
            cmd.args(["--network", network]);
        }
        cmd
    }

    fn launch(&self, line: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", line])
            .current_dir(&self.other)
            .env(
                "PATH",
                std::env::join_paths([
                    self.bin.clone(),
                    PathBuf::from("/usr/bin"),
                    PathBuf::from("/bin"),
                ])
                .unwrap(),
            )
            .env("HOME", &self.other)
            .env("XDG_CONFIG_HOME", &self.other)
            .env("XDG_DATA_HOME", &self.other)
            .env("SATS_DIR", &self.other)
            .env("SATS_AGENT_TOKEN", "wrong-ambient-token")
            .env_remove("SATS_PASSWORD")
            .env("NO_COLOR", "1")
            .env("SATS_FIXTURE_BINARY", env!("CARGO_BIN_EXE_sats"))
            .env(
                "SATS_FIXTURE_CODEX_ARGS",
                self.root.path().join("codex-args"),
            )
            .env(
                "SATS_FIXTURE_CLAUDE_ARGS",
                self.root.path().join("claude-args"),
            )
            .env("SATS_FIXTURE_SATS_ARGS", self.root.path().join("sats-args"))
            .env("SATS_FIXTURE_ENV", self.root.path().join("sats-env"));
        cmd
    }

    fn captured(&self, file: &str) -> Vec<String> {
        let bytes = fs::read(self.root.path().join(file)).unwrap();
        bytes
            .strip_suffix(&[0])
            .unwrap()
            .split(|b| *b == 0)
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
            .collect()
    }
}

struct Mcp {
    child: Child,
    incoming: Receiver<Value>,
}

impl Mcp {
    fn start(mut cmd: Command) -> Self {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = child.stdout.take().unwrap();
        let (sender, incoming) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                if let Ok(value) = serde_json::from_str(&line)
                    && sender.send(value).is_err()
                {
                    break;
                }
            }
        });
        let mut mcp = Self { child, incoming };
        mcp.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"connection-test","version":"0"}
        }}));
        assert_eq!(mcp.recv()["id"], 1);
        mcp.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        mcp
    }

    fn send(&mut self, value: Value) {
        writeln!(self.child.stdin.as_mut().unwrap(), "{value}").unwrap();
        self.child.stdin.as_mut().unwrap().flush().unwrap();
    }

    fn recv(&mut self) -> Value {
        match self.incoming.recv_timeout(Duration::from_secs(10)) {
            Ok(value) => value,
            Err(error) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
                let mut stderr = String::new();
                self.child
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut stderr)
                    .unwrap();
                panic!("MCP reply before deadline: {error}; {stderr}");
            }
        }
    }

    fn tool(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}));
        let response = self.recv();
        assert_eq!(response["id"], id);
        assert!(response.get("error").is_none(), "{response}");
        assert_ne!(response["result"]["isError"], true, "{response}");
        response["result"]["structuredContent"].clone()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn find_named(root: &Path, name: &str) -> PathBuf {
    fn walk(dir: &Path, name: &str, results: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, name, results);
            } else if path.file_name().unwrap() == name {
                results.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(root, name, &mut found);
    assert_eq!(found.len(), 1, "{found:?}");
    found.pop().unwrap()
}

fn embedded_token(line: &str) -> &str {
    line.split_ascii_whitespace()
        .find_map(|word| word.strip_prefix("SATS_AGENT_TOKEN="))
        .unwrap()
}

fn check_connection(
    network: &str,
    directory: Option<&str>,
    xdg: bool,
    auth_errors: bool,
    agent: &str,
) {
    let fixture = Fixture::new();
    let directory = directory.map(|name| {
        if name.starts_with("./") {
            PathBuf::from(name)
        } else {
            fixture.root.path().join(name)
        }
    });
    let mut init = fixture.sats(directory.as_deref(), Some(network), xdg);
    let out = init.args(["init", "--words", "12"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seed = find_named(fixture.root.path(), "seed.sealed");
    let data = seed.parent().unwrap();
    // Find the actual config through the created wallet, without making test
    // assumptions about directories' platform-specific default layout.
    let config = if let Some(dir) = directory.as_deref() {
        if dir.is_absolute() {
            dir.join("config.toml")
        } else {
            fixture.cwd.join(dir).join("config.toml")
        }
    } else {
        let base = if xdg && !cfg!(target_os = "macos") {
            &fixture.config
        } else {
            &fixture.home
        };
        find_named(base, "config.toml")
    };
    // Config-selected network must be pinned just as an explicit override of
    // a conflicting configured network is.
    // Credentialed, and unresolvable: chain data is set to Subfrost, which
    // isn't set up.
    let providers = format!(
        "[{network}]\nchain = \"subfrost\"\n\n[{network}.esplora]\nurl = \"http://127.0.0.1:1/private-provider-secret\"\nbearer = \"private-bearer-secret\"\n"
    );
    let configured_network = if directory.is_some() {
        "mainnet"
    } else {
        network
    };
    fs::write(
        &config,
        format!("network = \"{configured_network}\"\n{providers}"),
    )
    .unwrap();
    let out = fixture
        .sats(
            directory.as_deref(),
            directory.as_ref().map(|_| network),
            xdg,
        )
        .args(["agent", "grant", "--budget", "12345", "--", agent])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout
            .lines()
            .any(|line| line == "Approve: sats agent approve")
    );
    assert!(
        stdout
            .lines()
            .any(|line| line == format!("Revoke:  sats agent revoke {agent}"))
    );
    let commands = [
        (
            "codex",
            stdout
                .lines()
                .find(|line| line.starts_with("codex mcp add "))
                .unwrap(),
            "codex-args",
        ),
        (
            "claude",
            stdout
                .lines()
                .find(|line| line.starts_with("claude mcp add "))
                .unwrap(),
            "claude-args",
        ),
    ];
    assert!(
        !stdout
            .lines()
            .any(|line| line.trim_start().starts_with("SATS_AGENT_TOKEN=")),
        "the token should only appear inside copyable commands"
    );
    let token = embedded_token(commands[0].1);
    assert_eq!(embedded_token(commands[1].1), token);
    for (_, line, _) in commands {
        assert!(!line.contains(common::PASSWORD));
        assert!(!line.contains("private-provider-secret"));
        assert!(!line.contains("private-bearer-secret"));
    }
    // The future client's defaults and this wallet's config may both change.
    // An unresolvable provider setup proves startup, filing and observation
    // stay local.
    fs::write(&config, format!("network = \"mainnet\"\n{providers}")).unwrap();
    let grant_path = data
        .join(network)
        .join("grants")
        .join(format!("{agent}.json"));
    let stored = fs::read_to_string(&grant_path).unwrap();
    assert!(
        !stored.contains(token),
        "bearer token must not be persisted"
    );
    let grant: sats_core::authz::Grant = serde_json::from_str(&stored).unwrap();
    assert!(grant.authorizes(token));

    let mut expected = vec!["--network".to_owned(), network.to_owned()];
    let shared_default = directory.is_none() && config.parent() == Some(data);
    let pinned_dir = if let Some(dir) = &directory {
        Some(if dir.is_absolute() {
            dir.clone()
        } else {
            fixture.cwd.canonicalize().unwrap().join(dir)
        })
    } else if shared_default {
        Some(data.to_path_buf())
    } else {
        None
    };
    if let Some(dir) = pinned_dir {
        let absolute = std::path::absolute(dir).unwrap();
        expected.extend(["--dir".into(), absolute.to_str().unwrap().into()]);
    }
    expected.extend(["agent".into(), "serve".into()]);
    if agent.starts_with('-') {
        expected.push("--".into());
    }
    expected.push(agent.into());

    for (client, line, args_file) in commands {
        let mut mcp = Mcp::start(fixture.launch(line));
        assert_eq!(fixture.captured("sats-args"), expected);
        assert!(!fixture.other.join("injected").exists());
        let client_args = fixture.captured(args_file);
        let token_arg = format!("SATS_AGENT_TOKEN={token}");
        match client {
            "codex" => {
                assert_eq!(&client_args[..4], ["mcp", "add", "sats", "--env"]);
                assert_eq!(client_args[4], token_arg);
                assert_eq!(client_args[5], "--");
            }
            "claude" => {
                assert_eq!(
                    &client_args[..8],
                    [
                        "mcp",
                        "add",
                        "--transport",
                        "stdio",
                        "--scope",
                        "local",
                        "sats",
                        "--env"
                    ]
                );
                assert_eq!(client_args[8], token_arg);
                assert_eq!(client_args[9], "--");
            }
            _ => unreachable!(),
        }
        let env = fixture.captured("sats-env");
        assert_eq!(env[4], token);
        if directory.is_none() && !shared_default {
            assert_eq!(env[0], "unset");
            assert_eq!(env[1], fixture.home.to_str().unwrap());
            assert!(Path::new(&env[2]).is_absolute());
            assert!(Path::new(&env[3]).is_absolute());
            #[cfg(not(target_os = "macos"))]
            {
                assert_eq!(
                    env[2],
                    config.parent().unwrap().parent().unwrap().to_str().unwrap()
                );
                assert_eq!(env[3], data.parent().unwrap().to_str().unwrap());
            }
        } else {
            assert_eq!(env[0], fixture.other.to_str().unwrap());
        }
        let view = mcp.tool(2, "get_grant", json!({}));
        assert_eq!(view["active"], true);
        assert_eq!(view["remaining_sat"], 12345);
        let address = mcp.tool(3, "get_receive_address", json!({}));
        assert_eq!(address["network"], network);
        let filed = mcp.tool(
            4,
            "request_send",
            json!({
                "address": address["address"],
                "amount_sat": 1000,
                "idempotency_key": format!("connection-{client}")
            }),
        );
        assert_eq!(filed["status"], "pending_approval");
        let id = filed["request_id"].as_str().unwrap();
        let path = data
            .join(network)
            .join("agent-requests")
            .join(agent)
            .join(format!("{id}.json"));
        let record: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(record["grant_id"], grant.grant_id);
        assert_eq!(
            mcp.tool(5, "check_request", json!({"request_id":id})),
            filed
        );
        drop(mcp);

        if auth_errors {
            for (mode, diagnostic) in [
                ("wrong", "does not match the active grant"),
                ("missing", "no SATS_AGENT_TOKEN"),
            ] {
                let out = fixture
                    .launch(line)
                    .env("SATS_FIXTURE_TOKEN_MODE", mode)
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                assert!(!out.status.success());
                assert!(
                    String::from_utf8_lossy(&out.stderr).contains(diagnostic),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                assert!(out.stdout.is_empty());
            }
        }
    }
}

#[test]
fn generated_connection_preserves_nondefault_network_and_default_layout() {
    check_connection("regtest", None, false, true, AGENT);
}

#[test]
fn generated_connection_preserves_custom_directory() {
    check_connection("signet", Some("wallet"), false, false, AGENT);
}

#[test]
fn generated_connection_preserves_network_and_quoted_directory() {
    check_connection(
        "regtest",
        Some("wallet ' $HOME $(touch injected) ; & `literal`"),
        false,
        false,
        AGENT,
    );
}

#[test]
fn generated_connection_resolves_relative_directory_before_cwd_changes() {
    check_connection(
        "regtest",
        Some("./wallet ' $HOME $(touch injected) ; & `literal`"),
        false,
        false,
        AGENT,
    );
}

#[test]
fn generated_connection_preserves_xdg_default_layout() {
    check_connection("testnet4", None, true, false, AGENT);
}

#[test]
fn generated_connection_preserves_agent_names_starting_with_a_hyphen() {
    check_connection("regtest", Some("wallet"), false, false, "-agent");
}

#[test]
fn observe_grant_omits_the_approval_hint() {
    let fixture = Fixture::new();
    let wallet = fixture.root.path().join("observe-wallet");
    let out = fixture
        .sats(Some(&wallet), Some("signet"), false)
        .args(["init", "--words", "12"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = fixture
        .sats(Some(&wallet), Some("signet"), false)
        .args([
            "agent", "grant", "observer", "--budget", "12345", "--mode", "observe",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("codex mcp add "))
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("claude mcp add "))
    );
    assert!(!stdout.contains("Approve: sats agent approve"));
    assert!(stdout.contains("Revoke:  sats agent revoke observer"));
}
