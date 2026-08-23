//! MCP server integration tests: raw JSON-RPC over the child process's
//! stdio. Fully offline — the spend tests run against the file-driven mock
//! provider with a directly funded wallet, exercising the full agent path:
//! send, attribution, idempotent retry, and persisted denials.
#![cfg(feature = "mcp")]

mod common;

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
            .args(["agent", "serve", agent])
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

fn handshake(mcp: &mut McpSession) {
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

/// A wallet the served process can actually spend from: mock provider
/// config plus directly seeded confirmed UTXOs.
fn funded_setup(dir: &TempDir, grant_args: &[&str], values_sat: &[u64]) -> std::path::PathBuf {
    run_sats(dir, &["init"]);
    let mockdata = common::write_mock_provider(dir);
    let mut args = vec!["agent", "grant", "claude"];
    args.extend_from_slice(grant_args);
    run_sats(dir, &args);
    common::fund_wallet(dir, values_sat);
    mockdata
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap())
        .unwrap_or_else(|e| panic!("bad json at {}: {e}", path.display()))
}

fn event_kinds(dir: &TempDir) -> Vec<String> {
    let log = std::fs::read_to_string(dir.path().join("signet/events/log.jsonl")).unwrap();
    log.lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["event"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[test]
fn mcp_send_succeeds_and_is_attributed() {
    let dir = TempDir::new().unwrap();
    let mockdata = funded_setup(
        &dir,
        &["--budget", "50000", "--max-tx", "30000"],
        &[100_000],
    );

    let mut mcp = McpSession::start(&dir, "claude");
    handshake(&mut mcp);
    let sent = mcp.call_tool(
        2,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 25_000, "request_id": "job-1" }),
    );
    assert_eq!(sent["status"], "sent", "got: {sent}");
    assert_eq!(sent["amount_sat"], 25_000);
    assert_eq!(sent["request_id"], "k-job-1");
    let txid = sent["txid"].as_str().unwrap().to_string();

    // The transaction reached the mock chain and its record names the
    // agent, the request, and the intent.
    let broadcasts = std::fs::read_to_string(mockdata.join("broadcasts.log")).unwrap();
    assert_eq!(broadcasts.lines().count(), 1);
    let record = read_json(
        &dir.path()
            .join("signet/transactions")
            .join(format!("{txid}.json")),
    );
    assert_eq!(record["status"], "broadcast");
    assert_eq!(record["origin"]["surface"], "mcp");
    assert_eq!(record["origin"]["agent"], "claude");
    assert_eq!(record["origin"]["request_id"], "k-job-1");
    assert_eq!(
        record["origin"]["intent_digest"].as_str().unwrap().len(),
        64
    );

    // The request record resolved to the same truth.
    let request = read_json(&dir.path().join("signet/agent-requests/claude/k-job-1.json"));
    assert_eq!(request["outcome"]["status"], "sent");
    assert_eq!(request["outcome"]["txid"], txid.as_str());
    assert_eq!(request["client_request_id"], "job-1");

    // And the causal chain is complete.
    assert_eq!(
        event_kinds(&dir),
        ["request_received", "reserved", "signed", "broadcast"]
    );
}

#[test]
fn mcp_send_same_key_replays_without_double_spend() {
    let dir = TempDir::new().unwrap();
    let mockdata = funded_setup(&dir, &["--budget", "50000"], &[100_000]);

    let mut mcp = McpSession::start(&dir, "claude");
    handshake(&mut mcp);
    let params = serde_json::json!({
        "address": ADDRESS, "amount_sat": 25_000, "request_id": "job-1",
    });
    let first = mcp.call_tool(2, "send", params.clone());
    assert_eq!(first["status"], "sent", "got: {first}");

    // The identical retry replays the recorded outcome: same txid, no new
    // broadcast, no second budget draw.
    let second = mcp.call_tool(3, "send", params);
    assert_eq!(second["status"], "sent", "got: {second}");
    assert_eq!(second["txid"], first["txid"]);
    assert_eq!(
        second["remaining_budget_sat"],
        first["remaining_budget_sat"]
    );
    let broadcasts = std::fs::read_to_string(mockdata.join("broadcasts.log")).unwrap();
    assert_eq!(broadcasts.lines().count(), 1, "no double broadcast");
    let transactions = std::fs::read_dir(dir.path().join("signet/transactions"))
        .unwrap()
        .count();
    assert_eq!(transactions, 1, "no second transaction");
    let grant = mcp.call_tool(4, "get_grant", serde_json::json!({}));
    assert_eq!(grant["remaining_sat"], first["remaining_budget_sat"]);

    // The same key for a different send is a typed conflict, not a spend.
    let conflict = mcp.call_tool(
        5,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 26_000, "request_id": "job-1" }),
    );
    assert_eq!(conflict["status"], "error", "got: {conflict}");
    assert_eq!(conflict["error_code"], "request_id_conflict");
}

#[test]
fn mcp_denial_carries_request_id_and_persists_request() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    run_sats(
        &dir,
        &[
            "agent", "grant", "claude", "--budget", "50000", "--max-tx", "10000",
        ],
    );

    let mut mcp = McpSession::start(&dir, "claude");
    handshake(&mut mcp);
    let denial = mcp.call_tool(
        2,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 20_000, "request_id": "big-1" }),
    );
    assert_eq!(denial["status"], "denied");
    assert_eq!(denial["reason"], "over_max_tx");
    assert_eq!(denial["request_id"], "k-big-1");

    // The denied request is on disk for the human review queue.
    let request = read_json(&dir.path().join("signet/agent-requests/claude/k-big-1.json"));
    assert_eq!(request["outcome"]["status"], "denied");
    assert_eq!(request["outcome"]["deny"]["reason"], "over_max_tx");
    assert_eq!(event_kinds(&dir), ["request_received", "denied"]);
}

#[test]
fn mcp_keyless_sends_never_replay() {
    let dir = TempDir::new().unwrap();
    let mockdata = funded_setup(&dir, &["--budget", "60000"], &[100_000]);

    let mut mcp = McpSession::start(&dir, "claude");
    handshake(&mut mcp);
    let params = serde_json::json!({ "address": ADDRESS, "amount_sat": 25_000 });
    let first = mcp.call_tool(2, "send", params.clone());
    let second = mcp.call_tool(3, "send", params);
    assert_eq!(first["status"], "sent", "got: {first}");
    assert_eq!(second["status"], "sent", "got: {second}");
    assert_ne!(first["txid"], second["txid"]);
    let broadcasts = std::fs::read_to_string(mockdata.join("broadcasts.log")).unwrap();
    assert_eq!(broadcasts.lines().count(), 2);
}

#[test]
fn mcp_serves_tools_and_enforces_the_grant() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    run_sats(
        &dir,
        &[
            "agent", "grant", "claude", "--budget", "50000", "--max-tx", "10000",
        ],
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
        .args(["agent", "serve", "nobody"])
        .env("SATS_DIR", dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no grant for \"nobody\""), "got: {stderr}");
    assert!(stderr.contains("sats agent grant nobody"), "got: {stderr}");
}

#[test]
fn revocation_takes_effect_mid_session() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    run_sats(&dir, &["agent", "grant", "claude", "--budget", "50000"]);

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
    run_sats(&dir, &["agent", "revoke", "claude"]);
    let denial = mcp.call_tool(
        2,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 1000 }),
    );
    assert_eq!(denial["status"], "denied");
    assert_eq!(denial["reason"], "revoked");
}

#[test]
fn approval_loop_end_to_end() {
    let dir = TempDir::new().unwrap();
    funded_setup(
        &dir,
        &["--budget", "50000", "--max-tx", "10000"],
        &[100_000],
    );

    let mut mcp = McpSession::start(&dir, "claude");
    handshake(&mut mcp);
    let params = serde_json::json!({
        "address": ADDRESS, "amount_sat": 20_000, "request_id": "big-1",
    });

    // Denied over the per-tx cap; the message points the human at the
    // exact one-time approval command.
    let denied = mcp.call_tool(2, "send", params.clone());
    assert_eq!(denied["status"], "denied");
    assert_eq!(denied["reason"], "over_max_tx");
    assert_eq!(denied["request_id"], "k-big-1");
    assert!(
        denied["message"]
            .as_str()
            .unwrap()
            .contains("sats agent approve k-big-1"),
        "got: {denied}"
    );

    // The human approves exactly this request, once.
    run_sats(&dir, &["agent", "approve", "k-big-1", "--max-fee", "5000"]);

    // The same keyed retry now succeeds, via the approval.
    let sent = mcp.call_tool(3, "send", params);
    assert_eq!(sent["status"], "sent", "got: {sent}");
    assert_eq!(sent["via_approval"], true);
    let request = read_json(&dir.path().join("signet/agent-requests/claude/k-big-1.json"));
    assert!(request["approval"]["consumed_at"].is_u64());
    assert_eq!(request["approval"]["consumed_by_request"], "k-big-1");

    // Single use: the same over-cap send under a fresh key denies again.
    let again = mcp.call_tool(
        4,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 20_000, "request_id": "big-2" }),
    );
    assert_eq!(again["status"], "denied", "got: {again}");
    assert_eq!(again["reason"], "over_max_tx");
}

#[test]
fn approval_fee_ceiling_denies_typed() {
    let dir = TempDir::new().unwrap();
    funded_setup(
        &dir,
        &["--budget", "50000", "--max-tx", "10000"],
        &[100_000],
    );

    let mut mcp = McpSession::start(&dir, "claude");
    handshake(&mut mcp);
    let params = serde_json::json!({
        "address": ADDRESS, "amount_sat": 20_000, "request_id": "cap-1",
    });
    let denied = mcp.call_tool(2, "send", params.clone());
    assert_eq!(denied["status"], "denied");

    // A one-sat ceiling can never cover a real fee.
    run_sats(&dir, &["agent", "approve", "k-cap-1", "--max-fee", "1"]);
    let over = mcp.call_tool(3, "send", params);
    assert_eq!(over["status"], "denied", "got: {over}");
    assert_eq!(over["reason"], "approval_fee_exceeded");
    // The ceiling check ran before consumption: the approval survives.
    let request = read_json(&dir.path().join("signet/agent-requests/claude/k-cap-1.json"));
    assert!(request["approval"]["consumed_at"].is_null());
}

#[test]
fn approval_does_not_override_revocation() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    run_sats(
        &dir,
        &[
            "agent", "grant", "claude", "--budget", "50000", "--max-tx", "10000",
        ],
    );

    let mut mcp = McpSession::start(&dir, "claude");
    handshake(&mut mcp);
    let params = serde_json::json!({
        "address": ADDRESS, "amount_sat": 20_000, "request_id": "rev-1",
    });
    let denied = mcp.call_tool(2, "send", params.clone());
    assert_eq!(denied["status"], "denied");
    run_sats(&dir, &["agent", "approve", "k-rev-1", "--max-fee", "5000"]);
    run_sats(&dir, &["agent", "revoke", "claude"]);

    let after = mcp.call_tool(3, "send", params);
    assert_eq!(after["status"], "denied", "got: {after}");
    assert_eq!(after["reason"], "revoked");
}

#[test]
fn approval_does_not_override_grant_expiry() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    run_sats(
        &dir,
        &[
            "agent", "grant", "claude", "--budget", "50000", "--max-tx", "10000",
        ],
    );

    let mut mcp = McpSession::start(&dir, "claude");
    handshake(&mut mcp);
    let params = serde_json::json!({
        "address": ADDRESS, "amount_sat": 20_000, "request_id": "exp-1",
    });
    let denied = mcp.call_tool(2, "send", params.clone());
    assert_eq!(denied["status"], "denied");
    run_sats(&dir, &["agent", "approve", "k-exp-1", "--max-fee", "5000"]);

    // Force the grant past its expiry; the armed approval must not save it.
    let grant_path = dir.path().join("signet/grants/claude.json");
    let mut grant = read_json(&grant_path);
    grant["expires_at"] = serde_json::json!(1);
    std::fs::write(&grant_path, grant.to_string()).unwrap();

    let after = mcp.call_tool(3, "send", params);
    assert_eq!(after["status"], "denied", "got: {after}");
    assert_eq!(after["reason"], "expired");
}

#[test]
fn keyed_retry_consumes_an_approval_held_by_a_keyless_request() {
    let dir = TempDir::new().unwrap();
    funded_setup(
        &dir,
        &["--budget", "50000", "--max-tx", "10000"],
        &[100_000],
    );

    let mut mcp = McpSession::start(&dir, "claude");
    handshake(&mut mcp);
    // A keyless request gets denied and holds the approval.
    let denied = mcp.call_tool(
        2,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 20_000 }),
    );
    assert_eq!(denied["status"], "denied");
    let holder_id = denied["request_id"].as_str().unwrap().to_string();
    run_sats(&dir, &["agent", "approve", &holder_id, "--max-fee", "5000"]);

    // A keyed retry of the same intent finds and consumes it.
    let sent = mcp.call_tool(
        3,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 20_000, "request_id": "big-9" }),
    );
    assert_eq!(sent["status"], "sent", "got: {sent}");
    assert_eq!(sent["via_approval"], true);
    let holder = read_json(
        &dir.path()
            .join("signet/agent-requests/claude")
            .join(format!("{holder_id}.json")),
    );
    assert_eq!(holder["approval"]["consumed_by_request"], "k-big-9");
}
