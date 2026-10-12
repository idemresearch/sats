//! Driving the MCP server as an agent would: raw JSON-RPC over the
//! served process's stdio, plus the human's side of the loop through the
//! CLI. Shared by the MCP integration tests and the scripted demo.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use tempfile::TempDir;

pub use super::{ADDRESS, PASSWORD};

pub fn sats_bin() -> std::path::PathBuf {
    std::env::var_os("SATS_TEST_BINARY")
        .map(Into::into)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_sats").into())
}

pub fn sats_output(dir: &TempDir, args: &[&str]) -> std::process::Output {
    Command::new(sats_bin())
        .args(args)
        .env("SATS_DIR", dir.path())
        .env("SATS_PASSWORD", PASSWORD)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

pub fn run_sats(dir: &TempDir, args: &[&str]) {
    let output = sats_output(dir, args);
    assert!(
        output.status.success(),
        "sats {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Run a command that prints JSON on stdout and parse it.
pub fn sats_json(dir: &TempDir, args: &[&str]) -> serde_json::Value {
    let output = sats_output(dir, args);
    assert!(
        output.status.success(),
        "sats {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("json stdout")
}

/// The server request id a `request_send` result carries: `r-` plus 32
/// hex characters, the only handle a human approves or dismisses by.
pub fn id_of(view: &serde_json::Value) -> String {
    let id = view["request_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no request_id in {view}"))
        .to_string();
    assert_eq!(id.len(), 34, "{id}");
    assert!(id.starts_with("r-"), "{id}");
    assert!(id[2..].bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
    id
}

/// The human authorizes and executes one request: `sats agent approve`
/// with the password from the environment, as a script would.
pub fn approve(dir: &TempDir, id: &str) -> serde_json::Value {
    sats_json(dir, &["--json", "agent", "approve", id, "--yes"])
}

/// Issue a grant and capture the bearer token, which is printed once and
/// never stored — the only place a test can get it is here.
pub fn grant_token(dir: &TempDir, agent: &str, extra: &[&str]) -> String {
    let mut args = vec!["--json", "agent", "grant", agent];
    args.extend_from_slice(extra);
    let json = sats_json(dir, &args);
    json["token"]
        .as_str()
        .expect("grant --json must emit the token exactly once")
        .to_string()
}

pub struct McpSession {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
}

impl McpSession {
    pub fn start(dir: &TempDir, agent: &str, token: &str) -> McpSession {
        let mut child = Command::new(sats_bin())
            .args(["agent", "serve", agent])
            .env("SATS_DIR", dir.path())
            .env("SATS_AGENT_TOKEN", token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        McpSession { child, reader }
    }

    pub fn send(&mut self, msg: serde_json::Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }

    pub fn recv(&mut self) -> serde_json::Value {
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line).unwrap();
            assert!(n > 0, "mcp server closed stdout unexpectedly");
            if !line.trim().is_empty() {
                return serde_json::from_str(line.trim()).unwrap();
            }
        }
    }

    /// The whole JSON-RPC response, for calls expected to be rejected
    /// before a tool result exists.
    pub fn call_tool_raw(
        &mut self,
        id: u64,
        name: &str,
        arguments: serde_json::Value,
    ) -> serde_json::Value {
        self.send(serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        }));
        let response = self.recv();
        assert_eq!(response["id"], id);
        response
    }

    pub fn call_tool(
        &mut self,
        id: u64,
        name: &str,
        arguments: serde_json::Value,
    ) -> serde_json::Value {
        self.call_tool_raw(id, name, arguments)["result"]["structuredContent"].clone()
    }

    /// A read tool refused because the session's grant is no longer
    /// live: an `isError` result whose structured content names the code.
    pub fn refused_read(&mut self, id: u64, name: &str, code: &str) -> serde_json::Value {
        let response = self.call_tool_raw(id, name, serde_json::json!({}));
        assert_eq!(response["result"]["isError"], true, "{name}: {response}");
        let refused = response["result"]["structuredContent"].clone();
        assert_eq!(refused["status"], "error", "{name}: {refused}");
        assert_eq!(refused["error_code"], code, "{name}: {refused}");
        refused
    }

    pub fn request_send(&mut self, id: u64, amount_sat: u64, key: &str) -> serde_json::Value {
        self.call_tool(
            id,
            "request_send",
            serde_json::json!({ "address": ADDRESS, "amount_sat": amount_sat, "idempotency_key": key }),
        )
    }

    pub fn check_request(&mut self, id: u64, request_id: &str) -> serde_json::Value {
        self.call_tool(
            id,
            "check_request",
            serde_json::json!({ "request_id": request_id }),
        )
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn handshake(mcp: &mut McpSession) {
    mcp.send(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "smoke-test", "version": "0" },
        },
    }));
    mcp.recv();
    mcp.send(serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
}

/// A wallet the approving human can actually spend from: mock provider
/// config plus directly seeded confirmed UTXOs, and a granted agent.
pub struct Fixture {
    pub mockdata: std::path::PathBuf,
    pub token: String,
}

pub fn funded_setup(dir: &TempDir, grant_args: &[&str], values_sat: &[u64]) -> Fixture {
    run_sats(dir, &["init"]);
    let mockdata = super::write_mock_provider(dir);
    let token = grant_token(dir, "claude", grant_args);
    super::fund_wallet(dir, values_sat);
    Fixture { mockdata, token }
}

pub fn read_json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap())
        .unwrap_or_else(|e| panic!("bad json at {}: {e}", path.display()))
}

pub fn request_record(dir: &TempDir, agent: &str, id: &str) -> serde_json::Value {
    read_json(
        &dir.path()
            .join("signet/agent-requests")
            .join(agent)
            .join(format!("{id}.json")),
    )
}

pub fn spent_sat(dir: &TempDir) -> u64 {
    read_json(&dir.path().join("signet/grants/claude.json"))["spent_sat"]
        .as_u64()
        .unwrap()
}

pub fn event_kinds(dir: &TempDir) -> Vec<String> {
    let log =
        std::fs::read_to_string(dir.path().join("signet/events/log.jsonl")).unwrap_or_default();
    log.lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["event"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}
