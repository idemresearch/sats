//! MCP server smoke test: raw JSON-RPC over the child process's stdio.
//! Fully offline — exercises startup validation, tool listing, the grant
//! snapshot, and a deterministic denial.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use tempfile::TempDir;

const PASSWORD: &str = "integration-test-pw";
const ADDRESS: &str = "tb1pvlnw9n2zuefmxzwmuz0763uajw8nmaattkhd8002g3ekejjspxtshu2q9n";

fn sats_bin() -> &'static str {
    env!("CARGO_BIN_EXE_sats")
}

fn run_sats(dir: &TempDir, args: &[&str]) {
    let status = Command::new(sats_bin())
        .args(args)
        .env("SATS_DIR", dir.path())
        .env("SATS_PASSWORD", PASSWORD)
        .env("NO_COLOR", "1")
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "sats {args:?} failed");
}

struct McpSession {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
}

impl McpSession {
    fn start(dir: &TempDir, agent: &str) -> McpSession {
        let mut child = Command::new(sats_bin())
            .args(["mcp", "--agent", agent])
            .env("SATS_DIR", dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        McpSession { child, reader }
    }

    fn send(&mut self, msg: serde_json::Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }

    fn recv(&mut self) -> serde_json::Value {
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

    fn call_tool(
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
        response["result"]["structuredContent"].clone()
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_serves_tools_and_enforces_the_grant() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    run_sats(
        &dir,
        &["grant", "claude", "--budget", "50000", "--max-tx", "10000"],
    );

    let mut mcp = McpSession::start(&dir, "claude");

    // Handshake.
    mcp.send(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "smoke-test", "version": "0" },
        },
    }));
    let init = mcp.recv();
    assert_eq!(init["result"]["serverInfo"]["name"], "sats");
    mcp.send(serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    // All four tools are exposed.
    mcp.send(serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let list = mcp.recv();
    let mut names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["get_balance", "get_grant", "get_receive_address", "send"]
    );

    // The grant snapshot lets the agent plan.
    let grant = mcp.call_tool(3, "get_grant", serde_json::json!({}));
    assert_eq!(grant["active"], true);
    assert_eq!(grant["budget_sat"], 50000);
    assert_eq!(grant["remaining_sat"], 50000);
    assert_eq!(grant["max_tx_sat"], 10000);

    // Over the per-tx cap: denied deterministically, offline, as a
    // successful tool result.
    let denial = mcp.call_tool(
        4,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 20000 }),
    );
    assert_eq!(denial["status"], "denied");
    assert_eq!(denial["reason"], "over_max_tx");
    let message = denial["message"].as_str().unwrap();
    assert!(
        message.contains("human authorization required"),
        "got: {message}"
    );
    assert!(message.contains("20,000"), "got: {message}");
    assert!(message.contains("10,000"), "got: {message}");

    // Over the whole budget (amount alone): also an offline denial.
    let denial = mcp.call_tool(
        5,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 60000 }),
    );
    assert_eq!(denial["status"], "denied");

    // A fresh receive address works through MCP too.
    let address = mcp.call_tool(6, "get_receive_address", serde_json::json!({}));
    assert!(address["address"].as_str().unwrap().starts_with("tb1p"));
}

#[test]
fn mcp_refuses_to_start_without_a_grant() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let output = Command::new(sats_bin())
        .args(["mcp", "--agent", "nobody"])
        .env("SATS_DIR", dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no grant for \"nobody\""), "got: {stderr}");
    assert!(stderr.contains("sats grant nobody"), "got: {stderr}");
}

#[test]
fn revocation_takes_effect_mid_session() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    run_sats(&dir, &["grant", "claude", "--budget", "50000"]);

    let mut mcp = McpSession::start(&dir, "claude");
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

    // Revoke while the server is running: the very next send is denied.
    run_sats(&dir, &["revoke", "claude"]);
    let denial = mcp.call_tool(
        2,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 1000 }),
    );
    assert_eq!(denial["status"], "denied");
    assert_eq!(denial["reason"], "revoked");
}
