//! MCP server integration tests: raw JSON-RPC over the child process's
//! stdio. Fully offline — the spend tests run against the file-driven mock
//! provider with a directly funded wallet, exercising the whole request
//! loop: the agent files a request, the human approves it with the CLI,
//! the approving process executes, and the agent observes the result.
//!
//! The served process holds a bearer token and nothing else: it cannot
//! sign, and it takes no action after filing.
#![cfg(feature = "mcp")]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use tempfile::TempDir;

const PASSWORD: &str = "integration-test-pw";
const ADDRESS: &str = "tb1pvlnw9n2zuefmxzwmuz0763uajw8nmaattkhd8002g3ekejjspxtshu2q9n";

fn sats_bin() -> std::path::PathBuf {
    std::env::var_os("SATS_TEST_BINARY")
        .map(Into::into)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_sats").into())
}

fn sats_output(dir: &TempDir, args: &[&str]) -> std::process::Output {
    Command::new(sats_bin())
        .args(args)
        .env("SATS_DIR", dir.path())
        .env("SATS_PASSWORD", PASSWORD)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn run_sats(dir: &TempDir, args: &[&str]) {
    let output = sats_output(dir, args);
    assert!(
        output.status.success(),
        "sats {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Run a command that prints JSON on stdout and parse it.
fn sats_json(dir: &TempDir, args: &[&str]) -> serde_json::Value {
    let output = sats_output(dir, args);
    assert!(
        output.status.success(),
        "sats {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("json stdout")
}

/// The human authorizes and executes one request: `sats agent approve`
/// with the password from the environment, as a script would.
fn approve(dir: &TempDir, id: &str) -> serde_json::Value {
    sats_json(dir, &["--json", "agent", "approve", id, "--yes"])
}

/// Issue a grant and capture the bearer token, which is printed once and
/// never stored — the only place a test can get it is here.
fn grant_token(dir: &TempDir, agent: &str, extra: &[&str]) -> String {
    let mut args = vec!["--json", "agent", "grant", agent];
    args.extend_from_slice(extra);
    let json = sats_json(dir, &args);
    json["token"]
        .as_str()
        .expect("grant --json must emit the token exactly once")
        .to_string()
}

struct McpSession {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
}

impl McpSession {
    fn start(dir: &TempDir, agent: &str, token: &str) -> McpSession {
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

    fn request_send(&mut self, id: u64, amount_sat: u64, key: &str) -> serde_json::Value {
        self.call_tool(
            id,
            "request_send",
            serde_json::json!({ "address": ADDRESS, "amount_sat": amount_sat, "request_id": key }),
        )
    }

    fn check_request(&mut self, id: u64, key: &str) -> serde_json::Value {
        self.call_tool(
            id,
            "check_request",
            serde_json::json!({ "request_id": key }),
        )
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

/// A wallet the approving human can actually spend from: mock provider
/// config plus directly seeded confirmed UTXOs, and a granted agent.
struct Fixture {
    mockdata: std::path::PathBuf,
    token: String,
}

fn funded_setup(dir: &TempDir, grant_args: &[&str], values_sat: &[u64]) -> Fixture {
    run_sats(dir, &["init"]);
    let mockdata = common::write_mock_provider(dir);
    let token = grant_token(dir, "claude", grant_args);
    common::fund_wallet(dir, values_sat);
    Fixture { mockdata, token }
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap())
        .unwrap_or_else(|e| panic!("bad json at {}: {e}", path.display()))
}

fn request_record(dir: &TempDir, agent: &str, id: &str) -> serde_json::Value {
    read_json(
        &dir.path()
            .join("signet/agent-requests")
            .join(agent)
            .join(format!("{id}.json")),
    )
}

fn spent_sat(dir: &TempDir) -> u64 {
    read_json(&dir.path().join("signet/grants/claude.json"))["spent_sat"]
        .as_u64()
        .unwrap()
}

fn event_kinds(dir: &TempDir) -> Vec<String> {
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

#[test]
fn mcp_serves_five_tools_and_enforces_the_grant() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let token = grant_token(&dir, "claude", &["--budget", "50000", "--max-tx", "10000"]);

    let mut mcp = McpSession::start(&dir, "claude", &token);
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
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(
        instructions.contains("Agents create requests"),
        "{instructions}"
    );
    mcp.send(serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    // Exactly the five tools: the agent reads, files, and observes. It
    // cannot approve, unlock, sign, or broadcast.
    mcp.send(serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let list = mcp.recv();
    let tools = list["result"]["tools"].as_array().unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "check_request",
            "get_balance",
            "get_grant",
            "get_receive_address",
            "request_send",
        ]
    );
    let annotation = |name: &str, key: &str| {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap()
            .get("annotations")
            .and_then(|a| a.get(key))
            .cloned()
    };
    assert_eq!(
        annotation("check_request", "readOnlyHint"),
        Some(true.into())
    );
    assert_eq!(annotation("get_grant", "readOnlyHint"), Some(true.into()));
    assert_eq!(
        annotation("request_send", "readOnlyHint"),
        Some(false.into())
    );
    assert_eq!(
        annotation("request_send", "destructiveHint"),
        Some(false.into()),
        "filing a request moves no money"
    );
    assert_eq!(
        annotation("get_receive_address", "readOnlyHint"),
        Some(false.into()),
        "address derivation persists an index and is not read-only"
    );

    // The grant snapshot lets the agent plan.
    let grant = mcp.call_tool(3, "get_grant", serde_json::json!({}));
    assert_eq!(grant["active"], true);
    assert_eq!(grant["budget_sat"], 50000);
    assert_eq!(grant["remaining_sat"], 50000);
    assert_eq!(grant["max_tx_sat"], 10000);
    assert_eq!(grant["mode"], "ask");

    // Over the per-tx cap: a denied request, recorded, as a successful
    // tool result — terminal, with no approval path.
    let denied = mcp.request_send(4, 20_000, "big");
    assert_eq!(denied["status"], "denied", "got: {denied}");
    assert_eq!(denied["reason"], "over_max_tx");
    assert_eq!(denied["request_id"], "k-big");
    let message = denied["message"].as_str().unwrap();
    assert!(message.contains("outside the grant"), "got: {message}");
    assert!(message.contains("20,000"), "got: {message}");
    assert!(message.contains("10,000"), "got: {message}");
    assert_eq!(request_record(&dir, "claude", "k-big")["status"], "denied");
    assert_eq!(event_kinds(&dir), ["request_received", "denied"]);

    // A keyless request is denied the same way and gets a random id.
    let denied = mcp.call_tool(
        5,
        "request_send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 60000 }),
    );
    assert_eq!(denied["status"], "denied");
    assert_eq!(
        denied["reason"], "over_max_tx",
        "the cap answers before the budget"
    );
    assert!(denied["request_id"].as_str().unwrap().starts_with("r-"));

    // A wrong-network address is a typed operational error, not a record.
    let bad = mcp.call_tool(
        6,
        "request_send",
        serde_json::json!({ "address": "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4", "amount_sat": 1 }),
    );
    assert_eq!(bad["status"], "error");
    assert_eq!(bad["error_code"], "invalid_address");

    // A fresh receive address works through MCP too.
    let address = mcp.call_tool(7, "get_receive_address", serde_json::json!({}));
    assert!(address["address"].as_str().unwrap().starts_with("tb1p"));
}

/// The whole loop: the agent files, the human approves, the approving
/// process signs and broadcasts, the agent observes. The agent takes no
/// action after filing, and the served process never signs.
#[test]
fn request_send_files_pending_and_approve_executes() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(
        &dir,
        &["--budget", "50000", "--max-tx", "30000"],
        &[100_000],
    );
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);

    let filed = mcp.request_send(2, 4_500, "invoice-1");
    assert_eq!(filed["status"], "pending_approval", "got: {filed}");
    assert_eq!(filed["request_id"], "k-invoice-1");
    assert_eq!(filed["recipient"], ADDRESS);
    assert_eq!(filed["amount_sat"], 4_500);
    assert!(
        filed["message"]
            .as_str()
            .unwrap()
            .contains("sats agent approve k-invoice-1")
    );
    assert!(filed.get("txid").is_none());
    assert_eq!(event_kinds(&dir), ["request_received"]);
    assert!(
        !dir.path().join("signet/transactions").exists()
            || std::fs::read_dir(dir.path().join("signet/transactions"))
                .unwrap()
                .count()
                == 0,
        "filing signs nothing"
    );

    // Observing is free: the record and the log are untouched.
    let record_path = dir
        .path()
        .join("signet/agent-requests/claude/k-invoice-1.json");
    let record_before = std::fs::read(&record_path).unwrap();
    let events_before = std::fs::read(dir.path().join("signet/events/log.jsonl")).unwrap();
    for (id, key) in [(3, "invoice-1"), (4, "k-invoice-1"), (5, "invoice-1")] {
        let view = mcp.check_request(id, key);
        assert_eq!(view["status"], "pending_approval", "got: {view}");
        assert_eq!(view["request_id"], "k-invoice-1");
    }
    assert_eq!(std::fs::read(&record_path).unwrap(), record_before);
    assert_eq!(
        std::fs::read(dir.path().join("signet/events/log.jsonl")).unwrap(),
        events_before
    );

    // The human's queue shows it, and the human approves.
    let queue = sats_json(&dir, &["--json", "agent", "requests"]);
    assert_eq!(queue.as_array().unwrap().len(), 1);
    assert_eq!(queue[0]["id"], "k-invoice-1");
    assert_eq!(queue[0]["status"], "pending_approval");
    let approved = approve(&dir, "k-invoice-1");
    assert_eq!(approved["status"], "sent", "got: {approved}");
    let txid = approved["txid"].as_str().unwrap().to_string();
    assert_eq!(approved["amount_sat"], 4_500);
    let fee = approved["fee_sat"].as_u64().unwrap();
    assert!(fee > 0);
    assert_eq!(approved["remaining_budget_sat"], 50_000 - 4_500 - fee);

    // The agent observes the result without having done anything.
    let view = mcp.check_request(6, "invoice-1");
    assert_eq!(view["status"], "sent", "got: {view}");
    assert_eq!(view["txid"], txid);
    assert_eq!(view["fee_sat"], fee);

    // Attribution and accounting.
    let record = read_json(&dir.path().join(format!("signet/transactions/{txid}.json")));
    assert_eq!(record["status"], "broadcast");
    assert_eq!(record["origin"]["surface"], "agent");
    assert_eq!(record["origin"]["agent"], "claude");
    assert_eq!(record["origin"]["request_id"], "k-invoice-1");
    assert_eq!(spent_sat(&dir), 4_500 + fee);
    assert!(
        std::fs::read_to_string(fx.mockdata.join("broadcasts.log"))
            .unwrap()
            .contains(&txid)
    );
    assert_eq!(
        event_kinds(&dir),
        [
            "request_received",
            "approved",
            "reserved",
            "signed",
            "broadcast"
        ]
    );
    let log = sats_json(
        &dir,
        &["--json", "agent", "log", "--request", "k-invoice-1"],
    );
    assert_eq!(log.as_array().unwrap().len(), 5);

    // Filing the same key again returns the settled request; it does
    // not file a second one and never pays twice.
    let again = mcp.request_send(7, 4_500, "invoice-1");
    assert_eq!(again["status"], "sent");
    assert_eq!(again["txid"], txid);
    assert_eq!(event_kinds(&dir).len(), 5);
    assert_eq!(spent_sat(&dir), 4_500 + fee);

    // And a settled request cannot be approved again.
    let output = sats_output(&dir, &["agent", "approve", "k-invoice-1", "--yes"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("already sent"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn request_send_is_idempotent_and_conflicts_on_key_reuse() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--budget", "50000"], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);

    let first = mcp.request_send(2, 1_000, "job-1");
    assert_eq!(first["status"], "pending_approval");
    let again = mcp.request_send(3, 1_000, "job-1");
    assert_eq!(again["status"], "pending_approval");
    assert_eq!(again["request_id"], "k-job-1");
    assert_eq!(event_kinds(&dir), ["request_received"], "no second filing");

    let conflict = mcp.request_send(4, 2_000, "job-1");
    assert_eq!(conflict["status"], "error", "got: {conflict}");
    assert_eq!(conflict["error_code"], "request_id_conflict");
    assert_eq!(conflict["request_id"], "k-job-1");
    assert_eq!(
        request_record(&dir, "claude", "k-job-1")["amount_sat"],
        1_000,
        "the record keeps its intent"
    );
    assert_eq!(event_kinds(&dir), ["request_received", "conflicted"]);

    // Keyless requests each file separately.
    let a = mcp.call_tool(
        5,
        "request_send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 1_000 }),
    );
    let b = mcp.call_tool(
        6,
        "request_send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 1_000 }),
    );
    assert!(a["request_id"].as_str().unwrap().starts_with("r-"));
    assert_ne!(a["request_id"], b["request_id"]);
    let queue = sats_json(&dir, &["--json", "agent", "requests"]);
    assert_eq!(queue.as_array().unwrap().len(), 3);

    // A malformed key is a typed error, before anything touches the disk.
    let bad = mcp.request_send(7, 1_000, "../etc");
    assert_eq!(bad["status"], "error");
    assert_eq!(bad["error_code"], "invalid_request_id");
}

/// Dismissal and grant boundaries are terminal states the agent observes.
#[test]
fn dismissed_and_denied_requests_observe_as_terminal() {
    let dir = TempDir::new().unwrap();
    let other = common::foreign_address(sats_core::bitcoin::Network::Signet);
    let fx = funded_setup(&dir, &["--budget", "50000", "--to", ADDRESS], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);

    let filed = mcp.request_send(2, 1_000, "later");
    assert_eq!(filed["status"], "pending_approval");
    let dismissed = sats_json(&dir, &["--json", "agent", "dismiss", "k-later"]);
    assert_eq!(dismissed["status"], "dismissed");
    let view = mcp.check_request(3, "later");
    assert_eq!(view["status"], "dismissed", "got: {view}");
    assert!(view["message"].as_str().unwrap().contains("dismissed"));
    assert_eq!(event_kinds(&dir), ["request_received", "dismissed"]);
    // Dismissed is terminal for the human too.
    let output = sats_output(&dir, &["agent", "approve", "k-later", "--yes"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already dismissed"));
    let output = sats_output(&dir, &["agent", "dismiss", "k-later"]);
    assert!(!output.status.success());

    // A recipient off the allowlist is a hard boundary.
    let off = mcp.call_tool(
        4,
        "request_send",
        serde_json::json!({ "address": other, "amount_sat": 1_000, "request_id": "stranger" }),
    );
    assert_eq!(off["status"], "denied", "got: {off}");
    assert_eq!(off["reason"], "recipient_not_allowed");
    let output = sats_output(&dir, &["agent", "approve", "k-stranger", "--yes"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("recipient_not_allowed"));

    // Observe mode files nothing approvable.
    let observer = grant_token(&dir, "watcher", &["--budget", "50000", "--mode", "observe"]);
    let mut watcher = McpSession::start(&dir, "watcher", &observer);
    handshake(&mut watcher);
    let observe = watcher.request_send(2, 1_000, "peek");
    assert_eq!(observe["status"], "denied");
    assert_eq!(observe["reason"], "observe_only");
    let balance = watcher.call_tool(3, "get_balance", serde_json::json!({}));
    assert_eq!(balance["balance_sat"], 100_000, "reading still works");

    // Another agent cannot see this agent's records through the tool.
    let cross = watcher.check_request(4, "k-later");
    assert_eq!(cross["status"], "not_found", "got: {cross}");
    let malformed = mcp.check_request(5, "../../etc");
    assert_eq!(malformed["status"], "not_found");
    assert!(malformed["message"].as_str().unwrap().contains("malformed"));
}

#[test]
fn revocation_takes_effect_mid_session() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--budget", "50000"], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let filed = mcp.request_send(2, 1_000, "before");
    assert_eq!(filed["status"], "pending_approval");

    run_sats(&dir, &["agent", "revoke", "claude"]);

    // Filing is refused without a grant; nothing is written.
    let refused = mcp.request_send(3, 1_000, "after");
    assert_eq!(refused["status"], "error", "got: {refused}");
    assert_eq!(refused["error_code"], "no_grant");
    assert!(
        !dir.path()
            .join("signet/agent-requests/claude/k-after.json")
            .exists()
    );
    // Reading the grant reports it gone; observing the old request works.
    let grant = mcp.call_tool(4, "get_grant", serde_json::json!({}));
    assert_eq!(grant["active"], false);
    assert_eq!(mcp.check_request(5, "before")["status"], "pending_approval");
    // The pending request cannot execute without its grant: it is bound
    // to the revoked instance and becomes denied.
    let output = sats_output(&dir, &["agent", "approve", "k-before", "--yes"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("revoked"));
    assert_eq!(mcp.check_request(6, "before")["status"], "denied");
    assert_eq!(mcp.check_request(7, "before")["reason"], "revoked");
    assert_eq!(event_kinds(&dir), ["request_received", "denied"]);

    // A re-issued grant for the same agent never executes the old request.
    let fresh = grant_token(&dir, "claude", &["--budget", "50000"]);
    let output = sats_output(&dir, &["agent", "approve", "k-before", "--yes"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("denied revoked"));
    assert_eq!(spent_sat(&dir), 0);
    let mut fresh_mcp = McpSession::start(&dir, "claude", &fresh);
    handshake(&mut fresh_mcp);
    assert_eq!(
        fresh_mcp.request_send(2, 1_000, "after")["status"],
        "pending_approval"
    );
}

/// A signature that could not be broadcast is a distinct state: the
/// reservation stands, the request never signs again, and the human's
/// rebroadcast settles it.
#[test]
fn broadcast_failure_observes_as_broadcast_pending_until_rebroadcast() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--budget", "50000"], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    assert_eq!(
        mcp.request_send(2, 1_000, "flaky")["status"],
        "pending_approval"
    );

    std::fs::write(fx.mockdata.join("broadcast-fail"), "provider down").unwrap();
    let approved = approve(&dir, "k-flaky");
    assert_eq!(approved["status"], "broadcast_pending", "got: {approved}");
    let txid = approved["txid"].as_str().unwrap().to_string();
    assert!(
        approved["message"]
            .as_str()
            .unwrap()
            .contains("budget reserved")
    );
    let record = read_json(&dir.path().join(format!("signet/transactions/{txid}.json")));
    assert_eq!(record["status"], "pending");
    let spent = spent_sat(&dir);
    assert!(spent > 1_000);
    let view = mcp.check_request(3, "flaky");
    assert_eq!(view["status"], "broadcast_pending", "got: {view}");
    assert_eq!(view["txid"], txid);
    assert_eq!(
        event_kinds(&dir),
        [
            "request_received",
            "approved",
            "reserved",
            "signed",
            "broadcast_failed"
        ]
    );

    // Approving again never produces a second signature.
    let output = sats_output(&dir, &["agent", "approve", "k-flaky", "--yes"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("sats tx broadcast"));
    assert_eq!(
        std::fs::read_dir(dir.path().join("signet/transactions"))
            .unwrap()
            .count(),
        1
    );

    // The human retries the broadcast; the request settles to sent.
    std::fs::remove_file(fx.mockdata.join("broadcast-fail")).unwrap();
    run_sats(&dir, &["tx", "broadcast", &txid]);
    assert_eq!(mcp.check_request(4, "flaky")["status"], "sent");
    assert_eq!(spent_sat(&dir), spent, "no second draw");
    assert_eq!(request_record(&dir, "claude", "k-flaky")["status"], "sent");
    assert_eq!(event_kinds(&dir).last().unwrap(), "broadcast");
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
fn a_superseded_token_is_refused_at_startup() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--budget", "50000"], &[100_000]);
    let stale = fx.token.clone();
    let fresh = grant_token(&dir, "claude", &["--budget", "50000"]);
    assert_ne!(stale, fresh);

    let output = Command::new(sats_bin())
        .args(["agent", "serve", "claude"])
        .env("SATS_DIR", dir.path())
        .env("SATS_AGENT_TOKEN", &stale)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not match the active grant"),
        "got: {stderr}"
    );

    // The replacement token starts fine.
    let mut mcp = McpSession::start(&dir, "claude", &fresh);
    handshake(&mut mcp);
    let grant = mcp.call_tool(2, "get_grant", serde_json::json!({}));
    assert_eq!(grant["active"], true);
}

/// Serving without a token names the command that prints one.
#[test]
fn serving_without_a_token_says_where_to_get_one() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let _token = grant_token(&dir, "claude", &["--budget", "50000"]);

    let output = Command::new(sats_bin())
        .args(["agent", "serve", "claude"])
        .env("SATS_DIR", dir.path())
        .env_remove("SATS_AGENT_TOKEN")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("SATS_AGENT_TOKEN"), "got: {stderr}");
    assert!(stderr.contains("sats agent grant claude"), "got: {stderr}");
}

/// A served process whose grant was replaced mid-session speaks a token
/// the store no longer honors: it can file nothing and learns nothing.
#[test]
fn a_replaced_grant_refuses_the_old_session_without_writing() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--budget", "50000"], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    assert_eq!(
        mcp.request_send(2, 1_000, "ok-1")["status"],
        "pending_approval"
    );

    let _fresh = grant_token(&dir, "claude", &["--budget", "50000"]);
    let requests_before = std::fs::read_dir(dir.path().join("signet/agent-requests/claude"))
        .unwrap()
        .count();
    let refused = mcp.request_send(3, 1_000, "stale-1");
    assert_eq!(refused["status"], "error", "got: {refused}");
    assert_eq!(refused["error_code"], "unauthorized");
    assert!(refused.get("reason").is_none(), "not a policy denial");
    let requests_after = std::fs::read_dir(dir.path().join("signet/agent-requests/claude"))
        .unwrap()
        .count();
    assert_eq!(
        requests_before, requests_after,
        "an unauthorized caller must not create request records"
    );
}

#[test]
fn get_balance_reports_a_failed_sync() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--budget", "50000"], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let balance = mcp.call_tool(2, "get_balance", serde_json::json!({}));
    assert_eq!(balance["balance_sat"], 100_000);
    assert_eq!(balance["synced"], true);
    std::fs::write(fx.mockdata.join("sync-error"), "offline").unwrap();
    let balance = mcp.call_tool(3, "get_balance", serde_json::json!({}));
    assert_eq!(balance["synced"], false);
    assert_eq!(balance["balance_sat"], 100_000, "cached");
}

// Provider failures on the approving side: a stalled chain provider
// refuses to plan on stale state and leaves the request pending; a
// broadcast timeout after signing is broadcast_pending with the budget
// reserved. Credentials never reach the human's error text.

fn with_http_provider(dir: &TempDir, driver: &str, url: &str, capability: &str) {
    let path = dir.path().join("config.toml");
    let mut config: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let all = [
        "chain.sync",
        "chain.fees",
        "chain.broadcast",
        "guard.native",
    ];
    let remaining: Vec<_> = all
        .iter()
        .filter(|&&c| c != capability)
        .map(|s| toml::Value::String((*s).into()))
        .collect();
    config["providers"]["mock"]
        .as_table_mut()
        .unwrap()
        .insert("capabilities".into(), toml::Value::Array(remaining));
    config["providers"].as_table_mut().unwrap().insert("http".into(),
        toml::Value::try_from(serde_json::json!({"network":"signet","driver":driver,"url":url,"capabilities":[capability]})).unwrap());
    std::fs::write(path, toml::to_string(&config).unwrap()).unwrap();
}

fn assert_sync_timeout(driver: &str) {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--budget", "50000"], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    assert_eq!(
        mcp.request_send(2, 1_000, "timeout")["status"],
        "pending_approval"
    );
    drop(mcp);

    let http = common::HttpServer::start(|_| None);
    let url = format!("{}/PRIVATE_PATH_KEY", http.url);
    with_http_provider(&dir, driver, &url, "chain.sync");
    let started = std::time::Instant::now();
    let output = sats_output(&dir, &["agent", "approve", "k-timeout", "--yes"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("stale state"), "got: {stderr}");
    assert!(started.elapsed() < std::time::Duration::from_secs(45));
    if driver == "subfrost" {
        assert!(!stderr.contains("PRIVATE_PATH_KEY"), "got: {stderr}");
    }
    assert_eq!(http.requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(event_kinds(&dir), ["request_received"]);
    assert_eq!(
        request_record(&dir, "claude", "k-timeout")["status"],
        "pending_approval"
    );
    assert_eq!(spent_sat(&dir), 0);
}

#[test]
fn esplora_timeout_refuses_stale_state() {
    assert_sync_timeout("esplora");
}

#[test]
fn subfrost_timeout_refuses_stale_state_and_redacts_credentials() {
    assert_sync_timeout("subfrost");
}

#[test]
fn esplora_retryable_get_is_limited_to_two_retries() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--budget", "50000"], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    assert_eq!(
        mcp.request_send(2, 1_000, "fees")["status"],
        "pending_approval"
    );
    drop(mcp);

    let http = common::HttpServer::start(|request| {
        assert!(request.starts_with("GET /fee-estimates "));
        Some((503, "unavailable".into()))
    });
    with_http_provider(&dir, "esplora", &http.url, "chain.fees");
    let output = sats_output(&dir, &["agent", "approve", "k-fees", "--yes"]);
    assert!(!output.status.success());
    assert_eq!(http.requests.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(
        request_record(&dir, "claude", "k-fees")["status"],
        "pending_approval"
    );
    assert_eq!(spent_sat(&dir), 0);
}

#[test]
fn broadcast_timeout_keeps_signed_transaction_and_reserved_budget() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--budget", "50000"], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    assert_eq!(
        mcp.request_send(2, 1_000, "broadcast-timeout")["status"],
        "pending_approval"
    );

    let http = common::HttpServer::start(|request| {
        assert!(request.starts_with("POST /tx "));
        None
    });
    with_http_provider(&dir, "esplora", &http.url, "chain.broadcast");
    let approved = approve(&dir, "k-broadcast-timeout");
    assert_eq!(approved["status"], "broadcast_pending", "got: {approved}");
    let txid = approved["txid"].as_str().unwrap();
    let record = read_json(&dir.path().join(format!("signet/transactions/{txid}.json")));
    assert_eq!(record["status"], "pending");
    assert!(spent_sat(&dir) > 1_000);
    assert_eq!(
        event_kinds(&dir),
        [
            "request_received",
            "approved",
            "reserved",
            "signed",
            "broadcast_failed"
        ]
    );
    let view = mcp.check_request(3, "broadcast-timeout");
    assert_eq!(view["status"], "broadcast_pending");
    assert_eq!(view["txid"], txid);
    assert_eq!(http.requests.load(std::sync::atomic::Ordering::SeqCst), 1);
}
