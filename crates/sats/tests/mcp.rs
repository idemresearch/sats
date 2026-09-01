//! MCP server integration tests: raw JSON-RPC over the child process's
//! stdio. Fully offline — the spend tests run against the file-driven mock
//! provider with a directly funded wallet, exercising the full agent path:
//! send, attribution, idempotent retry, and persisted denials.
//!
//! Each test runs a real `satsd` against its own SATS_DIR, because the
//! served process cannot sign: it holds a bearer token and nothing else.
//! The assertions below are on the tool contract, which the daemon did
//! not change — same tools, same result fields, same denial codes.
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

/// A running signing daemon, unlocked, torn down with the test.
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
        // Poll for readiness rather than sleeping: binding is fast.
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
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            up
        });
        assert!(ready, "satsd did not start");
        run_sats(dir, &["daemon", "unlock"]);
        Daemon(child)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Issue a grant and capture the bearer token, which is printed once and
/// never stored — the only place a test can get it is here.
fn grant_token(dir: &TempDir, agent: &str, extra: &[&str]) -> String {
    let mut args = vec!["--json", "agent", "grant", agent];
    args.extend_from_slice(extra);
    let output = Command::new(sats_bin())
        .args(&args)
        .env("SATS_DIR", dir.path())
        .env("SATS_PASSWORD", PASSWORD)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "grant failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
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

#[test]
fn mcp_stays_available_and_recovers_without_reconnecting() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    common::write_mock_provider(&dir);
    common::fund_wallet(&dir, &[100_000]);
    let token = grant_token(&dir, "claude", &["--budget", "50000"]);
    arm_approval(&dir, "claude", ADDRESS, 1_000);
    let mut mcp = McpSession::start(&dir, "claude", &token);
    handshake(&mut mcp);
    let status = mcp.call_tool(2, "get_status", serde_json::json!({}));
    assert_eq!(status["daemon_state"], "unavailable");
    assert_eq!(status["error_code"], "daemon_unavailable");
    assert_eq!(status["network"], "signet");
    assert_eq!(status["agent"], "claude");
    let address = mcp.call_tool(3, "get_receive_address", serde_json::json!({}));
    assert!(address["address"].as_str().unwrap().starts_with("tb1p"));
    let payment =
        serde_json::json!({"address": ADDRESS, "amount_sat": 1000, "request_id": "recovery"});
    let failed = mcp.call_tool(4, "send", payment.clone());
    assert_eq!(failed["error_code"], "daemon_unavailable");
    assert!(failed.get("reason").is_none());
    let daemon = Daemon::start(&dir);
    run_sats(&dir, &["daemon", "lock"]);
    let status = mcp.call_tool(5, "get_status", serde_json::json!({}));
    assert_eq!(status["daemon_state"], "locked");
    let failed = mcp.call_tool(6, "send", payment.clone());
    assert_eq!(failed["error_code"], "wallet_locked");
    run_sats(&dir, &["daemon", "unlock"]);
    let status = mcp.call_tool(7, "get_status", serde_json::json!({}));
    assert_eq!(status["daemon_state"], "unlocked");
    assert!(status.get("error_code").is_none());
    let sent = mcp.call_tool(8, "send", payment);
    assert_eq!(sent["status"], "sent");
    drop(daemon);
    let status = mcp.call_tool(9, "get_status", serde_json::json!({}));
    assert_eq!(status["daemon_state"], "unavailable");
}

#[test]
fn mcp_status_probe_has_a_deadline() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let token = grant_token(&dir, "claude", &["--budget", "50000"]);
    let listener =
        std::os::unix::net::UnixListener::bind(dir.path().join("signet/d.sock")).unwrap();
    let (done, wait) = std::sync::mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let first = listener.accept().unwrap();
        let second = listener.accept().unwrap();
        let _ = wait.recv();
        drop((first, second));
    });
    let mut mcp = McpSession::start(&dir, "claude", &token);
    handshake(&mut mcp);
    let start = std::time::Instant::now();
    let status = mcp.call_tool(2, "get_status", serde_json::json!({}));
    assert_eq!(status["daemon_state"], "unavailable");
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    drop(done);
    server.join().unwrap();
}

#[test]
fn send_progress_reports_real_stages_and_stops_on_denial() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(
        &dir,
        &["--budget", "50000", "--max-tx", "10000"],
        &[100_000],
    );
    arm_approval(&dir, "claude", ADDRESS, 1_000);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    for (id, amount, expected) in [(2, 1000, "sent"), (3, 20000, "denied")] {
        mcp.send(
            serde_json::json!({"jsonrpc":"2.0", "id":id,"method":"tools/call",
            "params":{"name":"send","arguments":{"address":ADDRESS,"amount_sat":amount},
                "_meta":{"progressToken":"send-progress"}}}),
        );
        let mut stages = Vec::new();
        loop {
            let message = mcp.recv();
            if message.get("id").is_some() {
                assert_eq!(message["id"], id);
                assert_eq!(message["result"]["structuredContent"]["status"], expected);
                break;
            }
            assert_eq!(message["method"], "notifications/progress");
            assert_eq!(message["params"]["progressToken"], "send-progress");
            assert!(message["params"].get("total").is_none());
            stages.push(message["params"]["message"].as_str().unwrap().to_string());
            assert_eq!(
                message["params"]["progress"].as_f64().unwrap(),
                stages.len() as f64
            );
        }
        let expected = if amount == 1000 {
            vec![
                "Checking request",
                "Syncing wallet",
                "Protecting UTXOs",
                "Estimating fees",
                "Building transaction",
                "Authorizing and signing",
                "Broadcasting transaction",
            ]
        } else {
            vec!["Checking request"]
        };
        assert_eq!(stages, expected);
    }
}

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
    arm_approval(&dir, "claude", ADDRESS, 1_000);
    let http = common::HttpServer::start(|_| None);
    let url = format!("{}/PRIVATE_PATH_KEY", http.url);
    with_http_provider(&dir, driver, &url, "chain.sync");
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let started = std::time::Instant::now();
    let result = mcp.call_tool(
        2,
        "send",
        serde_json::json!({"address":ADDRESS,"amount_sat":1000,"request_id":"timeout"}),
    );
    assert_eq!(result["status"], "error");
    assert!(result["message"].as_str().unwrap().contains("stale state"));
    assert!(started.elapsed() < std::time::Duration::from_secs(45));
    if driver == "subfrost" {
        assert!(!result.to_string().contains("PRIVATE_PATH_KEY"));
    }
    assert_eq!(http.requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(event_kinds(&dir), ["request_received", "failed"]);
    assert_eq!(
        read_json(&dir.path().join("signet/grants/claude.json"))["spent_sat"],
        0
    );
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
    arm_approval(&dir, "claude", ADDRESS, 1_000);
    let http = common::HttpServer::start(|request| {
        assert!(request.starts_with("GET /fee-estimates "));
        Some((503, "unavailable".into()))
    });
    with_http_provider(&dir, "esplora", &http.url, "chain.fees");
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let result = mcp.call_tool(
        2,
        "send",
        serde_json::json!({"address":ADDRESS,"amount_sat":1000}),
    );
    assert_eq!(result["status"], "error");
    assert_eq!(http.requests.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(
        read_json(&dir.path().join("signet/grants/claude.json"))["spent_sat"],
        0
    );
}

#[test]
fn broadcast_timeout_keeps_signed_transaction_and_reserved_budget() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--budget", "50000"], &[100_000]);
    arm_approval(&dir, "claude", ADDRESS, 1_000);
    let http = common::HttpServer::start(|request| {
        assert!(request.starts_with("POST /tx "));
        None
    });
    with_http_provider(&dir, "esplora", &http.url, "chain.broadcast");
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let params =
        serde_json::json!({"address":ADDRESS,"amount_sat":1000,"request_id":"broadcast-timeout"});
    let result = mcp.call_tool(2, "send", params.clone());
    assert_eq!(result["status"], "error");
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains("budget reserved")
    );
    let request = read_json(
        &dir.path()
            .join("signet/agent-requests/claude/k-broadcast-timeout.json"),
    );
    let txid = request["outcome"]["txid"].as_str().unwrap();
    let record = read_json(&dir.path().join(format!("signet/transactions/{txid}.json")));
    assert_eq!(record["status"], "pending");
    let spent = read_json(&dir.path().join("signet/grants/claude.json"))["spent_sat"]
        .as_u64()
        .unwrap();
    assert!(spent > 1000);
    assert_eq!(
        event_kinds(&dir),
        [
            "request_received",
            "reserved",
            "approval_consumed",
            "signed",
            "broadcast_failed"
        ]
    );
    let replay = mcp.call_tool(3, "send", params);
    assert_eq!(replay, result);
    assert_eq!(http.requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        read_json(&dir.path().join("signet/grants/claude.json"))["spent_sat"],
        spent
    );
}

/// Everything a served agent needs: a funded wallet, a running daemon,
/// and the one-time token naming its grant.
struct Fixture {
    mockdata: std::path::PathBuf,
    token: String,
    _daemon: Daemon,
}

/// A wallet the served process can actually spend from: mock provider
/// config plus directly seeded confirmed UTXOs, behind a live daemon.
fn funded_setup(dir: &TempDir, grant_args: &[&str], values_sat: &[u64]) -> Fixture {
    run_sats(dir, &["init"]);
    let mockdata = common::write_mock_provider(dir);
    let token = grant_token(dir, "claude", grant_args);
    common::fund_wallet(dir, values_sat);
    let daemon = Daemon::start(dir);
    Fixture {
        mockdata,
        token,
        _daemon: daemon,
    }
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

/// Pre-arm a one-time human approval for an exact intent — the durable
/// record `sats agent approve` would leave behind — so the next matching
/// send can pass the terminal ask. Every agent send needs one now; there
/// is no autonomous mode. The approval binds the canonical intent digest
/// (network, agent, recipient, amount), exactly like the real command.
fn arm_approval(dir: &TempDir, agent: &str, address: &str, amount_sat: u64) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static ARMED: AtomicUsize = AtomicUsize::new(0);
    let n = ARMED.fetch_add(1, Ordering::SeqCst);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let digest = sats_core::intent::SendIntent {
        network: "signet".into(),
        agent: agent.into(),
        recipient: address.into(),
        amount_sat,
    }
    .digest();
    let id = format!("k-armed-{n}");
    let record = serde_json::json!({
        "format_version": 1,
        "id": id,
        "network": "signet",
        "agent": agent,
        "recipient": address,
        "amount_sat": amount_sat,
        "intent_digest": digest,
        "created_at": now,
        "updated_at": now,
        "outcome": {
            "status": "denied",
            "deny": { "reason": "ask_required" },
            "resolved_at": now,
        },
        "approval": {
            "intent_digest": digest,
            "approved_at": now,
            "expires_at": now + 3_600,
        },
    });
    let requests_dir = dir.path().join("signet/agent-requests").join(agent);
    std::fs::create_dir_all(&requests_dir).unwrap();
    std::fs::write(
        requests_dir.join(format!("{id}.json")),
        serde_json::to_vec_pretty(&record).unwrap(),
    )
    .unwrap();
}

#[test]
fn mcp_send_succeeds_and_is_attributed() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(
        &dir,
        &["--budget", "50000", "--max-tx", "30000"],
        &[100_000],
    );
    arm_approval(&dir, "claude", ADDRESS, 25_000);

    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
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
    let broadcasts = std::fs::read_to_string(fx.mockdata.join("broadcasts.log")).unwrap();
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
        [
            "request_received",
            "reserved",
            "approval_consumed",
            "signed",
            "broadcast"
        ]
    );
}

#[test]
fn mcp_send_same_key_replays_without_double_spend() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--budget", "50000"], &[100_000]);
    arm_approval(&dir, "claude", ADDRESS, 25_000);

    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
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
    let broadcasts = std::fs::read_to_string(fx.mockdata.join("broadcasts.log")).unwrap();
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
    let token = grant_token(&dir, "claude", &["--budget", "50000", "--max-tx", "10000"]);
    let _daemon = Daemon::start(&dir);

    let mut mcp = McpSession::start(&dir, "claude", &token);
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
    let fx = funded_setup(&dir, &["--budget", "60000"], &[100_000]);

    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let params = serde_json::json!({ "address": ADDRESS, "amount_sat": 25_000 });
    // Each keyless execution consumes its own single-use approval: the
    // second identical send re-executes rather than replaying, and needs
    // a fresh human decision.
    arm_approval(&dir, "claude", ADDRESS, 25_000);
    let first = mcp.call_tool(2, "send", params.clone());
    arm_approval(&dir, "claude", ADDRESS, 25_000);
    let second = mcp.call_tool(3, "send", params);
    assert_eq!(first["status"], "sent", "got: {first}");
    assert_eq!(second["status"], "sent", "got: {second}");
    assert_ne!(first["txid"], second["txid"]);
    let broadcasts = std::fs::read_to_string(fx.mockdata.join("broadcasts.log")).unwrap();
    assert_eq!(broadcasts.lines().count(), 2);
}

#[test]
fn mcp_serves_tools_and_enforces_the_grant() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let token = grant_token(&dir, "claude", &["--budget", "50000", "--max-tx", "10000"]);
    let _daemon = Daemon::start(&dir);

    let mut mcp = McpSession::start(&dir, "claude", &token);

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

    // The whole tool surface is exposed, and the annotations advertise
    // what each tool is — hints for clients, never the enforcement.
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
            "get_status",
            "request_unlock",
            "send"
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
    assert_eq!(annotation("send", "readOnlyHint"), Some(false.into()));
    assert_eq!(annotation("send", "destructiveHint"), Some(true.into()));
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
    let token = grant_token(&dir, "claude", &["--budget", "50000"]);
    let _daemon = Daemon::start(&dir);

    let mut mcp = McpSession::start(&dir, "claude", &token);
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
    // The denied keyless send wrote nothing: no record dir was created
    // for an agent whose grant no longer exists.
    assert!(
        !dir.path().join("signet/agent-requests/claude").exists(),
        "a no-grant send must not create request records"
    );
}

#[test]
fn approval_loop_end_to_end() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(
        &dir,
        &["--budget", "50000", "--max-tx", "10000"],
        &[100_000],
    );

    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let params = serde_json::json!({
        "address": ADDRESS, "amount_sat": 8_000, "request_id": "big-1",
    });

    // The novel in-grant send terminates in the ask; the message points
    // the human at the exact one-time approval command.
    let denied = mcp.call_tool(2, "send", params.clone());
    assert_eq!(denied["status"], "denied");
    assert_eq!(denied["reason"], "ask_required");
    assert_eq!(denied["request_id"], "k-big-1");
    assert!(
        denied["message"]
            .as_str()
            .unwrap()
            .contains("sats agent approve k-big-1"),
        "got: {denied}"
    );

    // The human approves exactly this request, once.
    run_sats(&dir, &["agent", "approve", "k-big-1"]);

    // The same keyed retry now succeeds, via the approval.
    let sent = mcp.call_tool(3, "send", params);
    assert_eq!(sent["status"], "sent", "got: {sent}");
    let request = read_json(&dir.path().join("signet/agent-requests/claude/k-big-1.json"));
    assert!(request["approval"]["consumed_at"].is_u64());
    assert_eq!(request["approval"]["consumed_by_request"], "k-big-1");

    // Single use: the identical send under a fresh key asks again.
    let again = mcp.call_tool(
        4,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 8_000, "request_id": "big-2" }),
    );
    assert_eq!(again["status"], "denied", "got: {again}");
    assert_eq!(again["reason"], "ask_required");
}

#[test]
fn approval_does_not_override_revocation() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let token = grant_token(&dir, "claude", &["--budget", "50000", "--max-tx", "10000"]);
    let _daemon = Daemon::start(&dir);

    let mut mcp = McpSession::start(&dir, "claude", &token);
    handshake(&mut mcp);
    let params = serde_json::json!({
        "address": ADDRESS, "amount_sat": 8_000, "request_id": "rev-1",
    });
    let denied = mcp.call_tool(2, "send", params.clone());
    assert_eq!(denied["status"], "denied");
    run_sats(&dir, &["agent", "approve", "k-rev-1"]);
    run_sats(&dir, &["agent", "revoke", "claude"]);

    let poll = mcp.call_tool(
        4,
        "check_request",
        serde_json::json!({ "request_id": "rev-1" }),
    );
    assert_eq!(poll["approval_ready"], false, "got: {poll}");
    assert!(
        poll["message"]
            .as_str()
            .unwrap()
            .contains("no active grant")
    );

    let after = mcp.call_tool(3, "send", params);
    assert_eq!(after["status"], "denied", "got: {after}");
    assert_eq!(after["reason"], "revoked");
}

#[test]
fn approval_does_not_override_grant_expiry() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let token = grant_token(&dir, "claude", &["--budget", "50000", "--max-tx", "10000"]);
    let _daemon = Daemon::start(&dir);

    let mut mcp = McpSession::start(&dir, "claude", &token);
    handshake(&mut mcp);
    let params = serde_json::json!({
        "address": ADDRESS, "amount_sat": 8_000, "request_id": "exp-1",
    });
    let denied = mcp.call_tool(2, "send", params.clone());
    assert_eq!(denied["status"], "denied");
    run_sats(&dir, &["agent", "approve", "k-exp-1"]);

    // Tighten the existing hard envelope without rewriting the historical
    // denial. Polling and approval creation must both re-read the policy.
    let grant_path = dir.path().join("signet/grants/claude.json");
    let original_grant = read_json(&grant_path);
    let request_path = dir.path().join("signet/agent-requests/claude/k-exp-1.json");
    let request_before = std::fs::read(&request_path).unwrap();
    let events_path = dir.path().join("signet/events/log.jsonl");
    let events_before = std::fs::read(&events_path).unwrap();
    for (field, value, reason) in [
        ("max_tx_sat", serde_json::json!(5_000), "over_max_tx"),
        (
            "allowed_recipients",
            serde_json::json!(["tb1pother"]),
            "recipient_not_allowed",
        ),
        ("expires_at", serde_json::json!(1), "expired"),
    ] {
        let mut grant = original_grant.clone();
        grant[field] = value;
        std::fs::write(&grant_path, grant.to_string()).unwrap();
        let poll = mcp.call_tool(
            4,
            "check_request",
            serde_json::json!({ "request_id": "exp-1" }),
        );
        assert_eq!(
            poll["reason"], "ask_required",
            "recorded history stays intact"
        );
        assert_eq!(poll["approval_ready"], false, "got: {poll}");
        assert!(
            poll["message"].as_str().unwrap().contains(reason),
            "got: {poll}"
        );
        assert!(!poll["message"].as_str().unwrap().contains("agent approve"));
        let refused = Command::new(sats_bin())
            .args(["agent", "approve", "k-exp-1"])
            .env("SATS_DIR", dir.path())
            .env("SATS_PASSWORD", "wrong")
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        assert!(!refused.status.success());
        assert!(String::from_utf8_lossy(&refused.stderr).contains(reason));
        assert_eq!(std::fs::read(&request_path).unwrap(), request_before);
        assert_eq!(std::fs::read(&events_path).unwrap(), events_before);
    }

    // The final policy above is expired; the armed approval cannot save it.
    let after = mcp.call_tool(3, "send", params);
    assert_eq!(after["status"], "denied", "got: {after}");
    assert_eq!(after["reason"], "expired");
}

#[test]
fn keyed_retry_consumes_an_approval_held_by_a_keyless_request() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(
        &dir,
        &["--budget", "50000", "--max-tx", "10000"],
        &[100_000],
    );

    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    // A keyless request gets denied and holds the approval.
    let denied = mcp.call_tool(
        2,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 8_000 }),
    );
    assert_eq!(denied["status"], "denied");
    let holder_id = denied["request_id"].as_str().unwrap().to_string();
    run_sats(&dir, &["agent", "approve", &holder_id]);

    // A keyed retry of the same intent finds and consumes it.
    let sent = mcp.call_tool(
        3,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 8_000, "request_id": "big-9" }),
    );
    assert_eq!(sent["status"], "sent", "got: {sent}");
    let holder = read_json(
        &dir.path()
            .join("signet/agent-requests/claude")
            .join(format!("{holder_id}.json")),
    );
    assert_eq!(holder["approval"]["consumed_by_request"], "k-big-9");
}

/// A locked daemon is not a policy denial. An agent must be able to tell
/// "your budget said no" from "no human has unlocked the wallet", because
/// only one of those is worth relaying to a human as a request to approve.
#[test]
fn a_locked_daemon_reports_locked_not_denied() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(
        &dir,
        &["--budget", "50000", "--max-tx", "30000"],
        &[100_000],
    );
    arm_approval(&dir, "claude", ADDRESS, 20_000);
    run_sats(&dir, &["daemon", "lock"]);

    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let result = mcp.call_tool(
        2,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 20_000, "request_id": "k-locked-1" }),
    );
    assert_eq!(result["status"], "error");
    assert_eq!(result["error_code"], "wallet_locked");
    assert!(result.get("reason").is_none(), "not a policy denial");
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains("sats daemon unlock"),
        "got: {result}"
    );

    // Nothing was spent, and the budget is untouched.
    let grant = read_json(&dir.path().join("signet/grants/claude.json"));
    assert_eq!(grant["spent_sat"], 0);
    assert_eq!(grant["tx_count"], 0);

    // Unlocking makes the identical retry work: the refusal was about
    // the wallet's state, not about this request.
    run_sats(&dir, &["daemon", "unlock"]);
    let sent = mcp.call_tool(
        3,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 20_000, "request_id": "k-locked-1" }),
    );
    assert_eq!(sent["status"], "sent", "got: {sent}");
}

/// Re-issuing a grant mints a new token, so the old one stops working —
/// rotation and revocation are the same act.
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

/// A token that no longer names this agent's grant is refused before any
/// record is written: an unauthenticated caller must not be able to leave
/// request records behind, and must not learn anything about the grant.
#[test]
fn a_wrong_token_writes_nothing() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--budget", "50000"], &[100_000]);
    let stale = fx.token.clone();
    // Replace the grant, then serve with the token that still matches the
    // *new* one so the process starts, and speak the stale one on the wire.
    let fresh = grant_token(&dir, "claude", &["--budget", "50000"]);
    arm_approval(&dir, "claude", ADDRESS, 1_000);

    let mut mcp = McpSession::start(&dir, "claude", &fresh);
    handshake(&mut mcp);

    // Sanity: the fresh token works.
    let ok = mcp.call_tool(
        2,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 1_000, "request_id": "k-ok-1" }),
    );
    assert_eq!(ok["status"], "sent", "got: {ok}");

    // Now drive the daemon directly with the stale token.
    let requests_before = std::fs::read_dir(dir.path().join("signet/agent-requests/claude"))
        .unwrap()
        .count();
    let outcome = daemon_begin_send(&dir, &stale, "k-stale-1", 1_000);
    assert_eq!(outcome["reply"], "outcome");
    assert_eq!(outcome["error_code"], "unauthorized");
    assert!(outcome.get("reason").is_none(), "not a policy denial");

    let requests_after = std::fs::read_dir(dir.path().join("signet/agent-requests/claude"))
        .unwrap()
        .count();
    assert_eq!(
        requests_before, requests_after,
        "an unauthorized caller must not create request records"
    );
}

/// Speak one `begin_send` to satsd directly, bypassing the shim, so the
/// daemon's own authentication can be tested rather than the shim's.
fn daemon_begin_send(
    dir: &TempDir,
    token: &str,
    request_id: &str,
    amount_sat: u64,
) -> serde_json::Value {
    daemon_begin_send_as(dir, "claude", token, request_id, amount_sat)
}

/// [`daemon_begin_send`] with the wire-level agent name under the
/// caller's control, for hostile-input tests.
fn daemon_begin_send_as(
    dir: &TempDir,
    agent: &str,
    token: &str,
    request_id: &str,
    amount_sat: u64,
) -> serde_json::Value {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let socket = dir.path().join("signet/d.sock");
    let mut stream = UnixStream::connect(&socket).unwrap();
    let request = serde_json::json!({
        "op": "begin_send",
        "protocol": 1,
        "token": token,
        "agent": agent,
        "request_id": request_id,
        "recipient": ADDRESS,
        "amount_sat": amount_sat,
    });
    writeln!(stream, "{request}").unwrap();
    stream.flush().unwrap();
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).unwrap();
    serde_json::from_str(line.trim()).unwrap()
}

/// The daemon writes nothing for an agent that has no grant: no request
/// record, no directory, no journal line. An unauthenticated caller must
/// not be able to grow the wallet's state.
#[test]
fn no_grant_begin_send_writes_nothing() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let token = grant_token(&dir, "claude", &["--budget", "50000"]);
    let _daemon = Daemon::start(&dir);

    let events_before =
        std::fs::read(dir.path().join("signet/events/log.jsonl")).unwrap_or_default();

    // "ghost" has no grant; the token is real but names another agent.
    let outcome = daemon_begin_send_as(&dir, "ghost", &token, "k-ghost-1", 1_000);
    assert_eq!(outcome["reply"], "outcome");
    assert_eq!(outcome["status"], "denied", "got: {outcome}");
    assert_eq!(outcome["reason"], "revoked");
    assert!(
        outcome.get("request_id").is_none(),
        "no record may exist to name: {outcome}"
    );

    assert!(
        !dir.path().join("signet/agent-requests/ghost").exists(),
        "a no-grant send must not create request records"
    );
    let events_after =
        std::fs::read(dir.path().join("signet/events/log.jsonl")).unwrap_or_default();
    assert_eq!(
        events_before, events_after,
        "a no-grant send must not journal"
    );
    // A keyless variant writes nothing either.
    let socket_alive = daemon_begin_send_as(&dir, "ghost2", &token, "k-g2", 1_000);
    assert_eq!(socket_alive["reason"], "revoked");
    assert!(
        !dir.path()
            .join("signet/agent-requests")
            .join("ghost2")
            .exists()
    );
}

/// An agent name is a path component; a traversal name is refused with a
/// typed operational error before the daemon touches the disk.
#[test]
fn a_traversal_agent_name_is_refused_before_any_io() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let token = grant_token(&dir, "claude", &["--budget", "50000"]);
    let _daemon = Daemon::start(&dir);

    for evil in ["../../evil", "/evil", "a/b", "..", "EVIL"] {
        let outcome = daemon_begin_send_as(&dir, evil, &token, "k-evil-1", 1_000);
        assert_eq!(outcome["reply"], "outcome");
        assert_eq!(outcome["error_code"], "invalid_agent", "got: {outcome}");
        assert!(outcome.get("reason").is_none(), "not a policy denial");
    }

    // Nothing landed anywhere: not outside the data dir, not inside it.
    assert!(!dir.path().parent().unwrap().join("evil").exists());
    assert!(!dir.path().join("evil").exists());
    assert!(!dir.path().join("signet/agent-requests").exists());
}

/// A persistent connection to satsd, for tests that span several calls
/// on one claim — `daemon_begin_send` opens a fresh connection per call,
/// which releases the claim it took.
struct DaemonConn {
    stream: std::os::unix::net::UnixStream,
    reader: BufReader<std::os::unix::net::UnixStream>,
}

impl DaemonConn {
    fn open(dir: &TempDir) -> DaemonConn {
        let stream = std::os::unix::net::UnixStream::connect(dir.path().join("signet/d.sock"))
            .expect("satsd socket");
        let reader = BufReader::new(stream.try_clone().unwrap());
        DaemonConn { stream, reader }
    }

    fn call(&mut self, request: serde_json::Value) -> serde_json::Value {
        writeln!(self.stream, "{request}").unwrap();
        self.stream.flush().unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }
}

/// The `Finish` token is enforced against the token that began the send:
/// a wrong one records nothing and keeps the claim, so the right token
/// can still close the request out on the same connection.
#[test]
fn finish_requires_the_token_that_began_the_send() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let token = grant_token(&dir, "claude", &["--budget", "50000"]);
    arm_approval(&dir, "claude", ADDRESS, 1_000);
    let _daemon = Daemon::start(&dir);

    let mut conn = DaemonConn::open(&dir);
    let begun = conn.call(serde_json::json!({
        "op": "begin_send", "protocol": 1, "token": token, "agent": "claude",
        "request_id": "fin-1", "recipient": ADDRESS, "amount_sat": 1_000,
    }));
    assert_eq!(begun["reply"], "proceed", "got: {begun}");
    assert_eq!(begun["request_id"], "k-fin-1");

    // Wrong and malformed tokens are refused with a typed operational
    // error — never a policy denial — and no outcome is recorded.
    for bad in ["11".repeat(32), "not-a-token".into()] {
        let refused = conn.call(serde_json::json!({
            "op": "finish", "protocol": 1, "token": bad,
            "outcome": { "broadcast": "failed", "message": "prep failed" },
        }));
        assert_eq!(refused["reply"], "outcome");
        assert_eq!(refused["error_code"], "unauthorized", "got: {refused}");
        assert!(refused.get("reason").is_none(), "not a policy denial");
        assert_eq!(refused["request_id"], "k-fin-1");
    }
    let record = read_json(&dir.path().join("signet/agent-requests/claude/k-fin-1.json"));
    assert!(
        record.get("outcome").is_none(),
        "a wrong token must not record an outcome: {record}"
    );

    // The claim survived the refusals: the begin token closes it out.
    let closed = conn.call(serde_json::json!({
        "op": "finish", "protocol": 1, "token": token,
        "outcome": { "broadcast": "failed", "message": "prep failed" },
    }));
    assert_eq!(closed["status"], "error", "got: {closed}");
    assert_eq!(closed["request_id"], "k-fin-1");
    let record = read_json(&dir.path().join("signet/agent-requests/claude/k-fin-1.json"));
    assert_eq!(record["outcome"]["status"], "failed");
    assert!(
        record["outcome"].get("txid").is_none(),
        "nothing was signed: {record}"
    );
    assert_eq!(event_kinds(&dir), ["request_received", "failed"]);
}

/// The two amount bands end to end: inside the grant, a proposal
/// terminates in the one approvable ask; over the hard per-tx cap it is
/// refused outright, carries no hint, and `sats agent approve` refuses
/// to arm one. Nothing executes unattended.
#[test]
fn amounts_split_into_ask_and_hard_deny() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(
        &dir,
        &["--budget", "1000000", "--max-tx", "10000"],
        &[2_000_000],
    );
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);

    // Inside the grant: at the cap, inclusive — the terminal ask, and
    // the approval loop executes it exactly once.
    let routine = mcp.call_tool(
        2,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 10_000, "request_id": "in-1" }),
    );
    assert_eq!(routine["status"], "denied", "got: {routine}");
    assert_eq!(routine["reason"], "ask_required");
    assert_eq!(routine["approvable"], true);
    assert!(
        routine["message"]
            .as_str()
            .unwrap()
            .contains("sats agent approve k-in-1")
    );
    run_sats(&dir, &["agent", "approve", "k-in-1"]);
    let sent = mcp.call_tool(
        3,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 10_000, "request_id": "in-1" }),
    );
    assert_eq!(sent["status"], "sent", "got: {sent}");
    assert!(sent.get("approvable").is_none(), "sent carries no verdict");

    // Over the cap: a grant boundary — denied, not approvable, no hint.
    let hard = mcp.call_tool(
        4,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 10_001, "request_id": "hard-1" }),
    );
    assert_eq!(hard["status"], "denied", "got: {hard}");
    assert_eq!(hard["reason"], "over_max_tx");
    assert_eq!(hard["approvable"], false);
    assert!(
        !hard["message"].as_str().unwrap().contains("agent approve"),
        "a hard denial must not invite an approval: {hard}"
    );

    // And the approve command refuses to arm one, before any password.
    let refused = Command::new(sats_bin())
        .args(["agent", "approve", "k-hard-1"])
        .env("SATS_DIR", dir.path())
        .env("SATS_PASSWORD", PASSWORD)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(!refused.status.success(), "hard denials cannot be approved");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("no approval can lift"), "stderr: {stderr}");
    // Nothing was armed: the record holds no approval.
    let record = read_json(
        &dir.path()
            .join("signet/agent-requests/claude/k-hard-1.json"),
    );
    assert!(record.get("approval").is_none());
}

/// check_request is the sanctioned wait: it reads the durable record,
/// reports approval readiness, and repeated polling writes nothing —
/// no events, no record changes, no cross-agent visibility.
#[test]
fn check_request_polls_without_side_effects() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(
        &dir,
        &["--budget", "100000", "--max-tx", "10000"],
        &[200_000],
    );
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);

    // Unknown and malformed ids are typed results, never errors.
    let missing = mcp.call_tool(
        2,
        "check_request",
        serde_json::json!({ "request_id": "never-sent" }),
    );
    assert_eq!(missing["found"], false, "got: {missing}");
    let malformed = mcp.call_tool(
        3,
        "check_request",
        serde_json::json!({ "request_id": "../../etc" }),
    );
    assert_eq!(malformed["found"], false);
    assert!(malformed["message"].as_str().unwrap().contains("malformed"));

    // A novel in-grant ask, then the poll loop around its approval.
    let denied = mcp.call_tool(
        4,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 8_000, "request_id": "poll-1" }),
    );
    assert_eq!(denied["status"], "denied", "got: {denied}");

    let record_path = dir
        .path()
        .join("signet/agent-requests/claude/k-poll-1.json");
    let record_before = std::fs::read(&record_path).unwrap();
    let events_before = std::fs::read(dir.path().join("signet/events/log.jsonl")).unwrap();
    // The bare client key resolves to the same record as the server id.
    for (id, key) in [(5, "poll-1"), (6, "k-poll-1"), (7, "poll-1")] {
        let poll = mcp.call_tool(
            id,
            "check_request",
            serde_json::json!({ "request_id": key }),
        );
        assert_eq!(poll["found"], true, "got: {poll}");
        assert_eq!(poll["request_id"], "k-poll-1");
        assert_eq!(poll["status"], "denied");
        assert_eq!(poll["reason"], "ask_required");
        assert_eq!(poll["approvable"], true);
        assert_eq!(poll["approval_ready"], false);
        assert!(poll["message"].as_str().unwrap().contains("agent approve"));
    }
    assert_eq!(
        std::fs::read(&record_path).unwrap(),
        record_before,
        "polling must not touch the record"
    );
    assert_eq!(
        std::fs::read(dir.path().join("signet/events/log.jsonl")).unwrap(),
        events_before,
        "polling must not journal"
    );

    // Approval flips the poll; the retry consumes it; the poll settles.
    run_sats(&dir, &["agent", "approve", "k-poll-1"]);
    let ready = mcp.call_tool(
        8,
        "check_request",
        serde_json::json!({ "request_id": "poll-1" }),
    );
    assert_eq!(ready["approval_ready"], true, "got: {ready}");
    assert!(ready["approval_expires_at"].as_u64().unwrap() > 0);
    assert!(
        ready["message"]
            .as_str()
            .unwrap()
            .contains("retry the identical send")
    );
    let sent = mcp.call_tool(
        9,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 8_000, "request_id": "poll-1" }),
    );
    assert_eq!(sent["status"], "sent", "got: {sent}");
    let settled = mcp.call_tool(
        10,
        "check_request",
        serde_json::json!({ "request_id": "poll-1" }),
    );
    assert_eq!(settled["status"], "sent");
    assert_eq!(settled["txid"], sent["txid"]);
    assert_eq!(settled["approval_ready"], false, "consumed");

    // A hard denial polls as not approvable.
    let hard = mcp.call_tool(
        11,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 30_000, "request_id": "hard-1" }),
    );
    assert_eq!(hard["reason"], "over_max_tx", "got: {hard}");
    let hard_poll = mcp.call_tool(
        12,
        "check_request",
        serde_json::json!({ "request_id": "hard-1" }),
    );
    assert_eq!(hard_poll["approvable"], false);
    assert!(
        hard_poll["message"]
            .as_str()
            .unwrap()
            .contains("not approvable")
    );

    // Another agent cannot see this agent's records through the tool.
    let ghost_token = grant_token(&dir, "ghost", &["--budget", "1000"]);
    let mut ghost = McpSession::start(&dir, "ghost", &ghost_token);
    handshake(&mut ghost);
    let cross = ghost.call_tool(
        2,
        "check_request",
        serde_json::json!({ "request_id": "k-poll-1" }),
    );
    assert_eq!(cross["found"], false, "got: {cross}");
}

#[test]
fn check_request_resolves_prefixed_keys_and_preserves_canonical_ids() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let token = grant_token(&dir, "claude", &["--budget", "50000", "--mode", "ask"]);
    let _daemon = Daemon::start(&dir);
    let mut mcp = McpSession::start(&dir, "claude", &token);
    handshake(&mut mcp);

    for key in [
        "k-job".to_string(),
        "r-job".to_string(),
        "k-".to_string(),
        "r-".to_string(),
        format!("k-{}", "x".repeat(62)),
        format!("r-{}", "x".repeat(62)),
    ] {
        let denied = mcp.call_tool(
            2,
            "send",
            serde_json::json!({ "address": ADDRESS, "amount_sat": 1000, "request_id": key }),
        );
        assert_eq!(denied["status"], "denied", "got: {denied}");
        let canonical = format!("k-{key}");
        assert_eq!(denied["request_id"], canonical);
        for query in [&key, &canonical] {
            let poll = mcp.call_tool(
                3,
                "check_request",
                serde_json::json!({ "request_id": query }),
            );
            assert_eq!(poll["found"], true, "query {query}: {poll}");
            assert_eq!(poll["request_id"], canonical);
        }
    }

    // A stored canonical id wins over a bare-key alias. The original
    // prefixed key remains reachable by the id its send returned.
    let denied = mcp.call_tool(
        4,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 2000, "request_id": "job" }),
    );
    assert_eq!(denied["request_id"], "k-job");
    for canonical in ["k-job", "k-k-job"] {
        let poll = mcp.call_tool(
            5,
            "check_request",
            serde_json::json!({ "request_id": canonical }),
        );
        assert_eq!(poll["request_id"], canonical, "got: {poll}");
    }

    // The same precedence applies to server-generated keyless ids.
    let keyless = mcp.call_tool(
        6,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 3000 }),
    );
    let random_id = keyless["request_id"].as_str().unwrap();
    assert!(random_id.starts_with("r-"));
    let keyed = mcp.call_tool(
        7,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 4000, "request_id": random_id }),
    );
    for canonical in [random_id, keyed["request_id"].as_str().unwrap()] {
        let poll = mcp.call_tool(
            8,
            "check_request",
            serde_json::json!({ "request_id": canonical }),
        );
        assert_eq!(poll["request_id"], canonical, "got: {poll}");
    }
}

/// Ask mode proposes everything and executes only through approvals;
/// observe mode reads but can never send; there is no autonomous mode.
/// Both modes flow through get_grant and take effect mid-session.
#[test]
fn modes_gate_the_send_path_end_to_end() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--budget", "50000", "--mode", "ask"], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);

    let grant = mcp.call_tool(2, "get_grant", serde_json::json!({}));
    assert_eq!(grant["mode"], "ask", "got: {grant}");

    // Every send asks, however small.
    let params = serde_json::json!({
        "address": ADDRESS, "amount_sat": 1_000, "request_id": "m-1",
    });
    let asked = mcp.call_tool(3, "send", params.clone());
    assert_eq!(asked["status"], "denied", "got: {asked}");
    assert_eq!(asked["reason"], "ask_required");
    assert_eq!(asked["approvable"], true);

    // The approval executes it exactly once; the next send asks again.
    run_sats(&dir, &["agent", "approve", "k-m-1"]);
    let sent = mcp.call_tool(4, "send", params);
    assert_eq!(sent["status"], "sent", "got: {sent}");
    let again = mcp.call_tool(
        5,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 1_000, "request_id": "m-2" }),
    );
    assert_eq!(again["reason"], "ask_required", "got: {again}");
    run_sats(&dir, &["agent", "approve", "k-m-2"]);

    // Observe: reads keep working, sends hard-deny, approvals refuse.
    run_sats(&dir, &["agent", "mode", "claude", "observe"]);
    let record_path = dir.path().join("signet/agent-requests/claude/k-m-2.json");
    let record_before = std::fs::read(&record_path).unwrap();
    let events_path = dir.path().join("signet/events/log.jsonl");
    let events_before = std::fs::read(&events_path).unwrap();
    let grant = mcp.call_tool(6, "get_grant", serde_json::json!({}));
    assert_eq!(grant["mode"], "observe");
    let balance = mcp.call_tool(7, "get_balance", serde_json::json!({}));
    assert!(balance["balance_sat"].as_u64().unwrap() > 0);
    let poll = mcp.call_tool(
        8,
        "check_request",
        serde_json::json!({ "request_id": "m-2" }),
    );
    assert_eq!(poll["found"], true);
    assert_eq!(
        poll["reason"], "ask_required",
        "recorded history stays intact"
    );
    assert_eq!(poll["approval_ready"], false, "got: {poll}");
    assert!(poll.get("approval_expires_at").is_none());
    assert!(poll["message"].as_str().unwrap().contains("observe_only"));
    assert!(!poll["message"].as_str().unwrap().contains("agent approve"));
    let refused = Command::new(sats_bin())
        .args(["agent", "approve", "k-m-2"])
        .env("SATS_DIR", dir.path())
        .env("SATS_PASSWORD", "wrong")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "an old ASK cannot override observe"
    );
    assert!(String::from_utf8_lossy(&refused.stderr).contains("observe_only"));
    assert_eq!(std::fs::read(&record_path).unwrap(), record_before);
    assert_eq!(std::fs::read(&events_path).unwrap(), events_before);
    let denied = mcp.call_tool(
        9,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 1_000, "request_id": "m-3" }),
    );
    assert_eq!(denied["reason"], "observe_only", "got: {denied}");
    assert_eq!(denied["approvable"], false);
    assert!(
        !denied["message"]
            .as_str()
            .unwrap()
            .contains("agent approve"),
        "observe denials carry no approval hint: {denied}"
    );
    let refused = Command::new(sats_bin())
        .args(["agent", "approve", "k-m-3"])
        .env("SATS_DIR", dir.path())
        .env("SATS_PASSWORD", PASSWORD)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(!refused.status.success(), "observe denials cannot be armed");

    // Restoring ask makes the unconsumed approval available again.
    run_sats(&dir, &["agent", "mode", "claude", "ask"]);
    let ready = mcp.call_tool(
        10,
        "check_request",
        serde_json::json!({ "request_id": "m-2" }),
    );
    assert_eq!(ready["approval_ready"], true, "got: {ready}");
    // A sibling approval never makes an already-sent request executable.
    let settled = mcp.call_tool(
        11,
        "check_request",
        serde_json::json!({ "request_id": "m-1" }),
    );
    assert_eq!(settled["status"], "sent");
    assert_eq!(settled["approval_ready"], false, "got: {settled}");
    let sent = mcp.call_tool(
        12,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 1_000, "request_id": "m-2" }),
    );
    assert_eq!(sent["status"], "sent", "got: {sent}");

    // The removed autonomous mode cannot come back through the CLI.
    let refused = Command::new(sats_bin())
        .args(["agent", "mode", "claude", "auto"])
        .env("SATS_DIR", dir.path())
        .env("SATS_PASSWORD", PASSWORD)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(!refused.status.success(), "auto does not exist");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("unknown mode"));
}

/// The standing allowlist is a hard grant boundary: listed recipients
/// are routine asks, strangers are refused outright with no approval
/// path, and only editing the allowlist — never an approval or payment
/// history — changes the list.
#[test]
fn recipient_allowlist_gates_standing_authority() {
    use bdk_wallet::bitcoin::Network;

    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--budget", "100000", "--to", ADDRESS], &[200_000]);
    let stranger = common::foreign_address(Network::Signet);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);

    // Listed: a routine ask, and the approval loop executes it.
    let routine = mcp.call_tool(
        2,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 1_000, "request_id": "r-listed" }),
    );
    assert_eq!(routine["reason"], "ask_required", "got: {routine}");
    run_sats(&dir, &["agent", "approve", "k-r-listed"]);
    let sent = mcp.call_tool(
        9,
        "send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 1_000, "request_id": "r-listed" }),
    );
    assert_eq!(sent["status"], "sent", "got: {sent}");

    // A stranger is a grant boundary: refused, not approvable, no hint.
    let params = serde_json::json!({
        "address": stranger, "amount_sat": 1_000, "request_id": "r-1",
    });
    let refused = mcp.call_tool(3, "send", params.clone());
    assert_eq!(refused["status"], "denied", "got: {refused}");
    assert_eq!(refused["reason"], "recipient_not_allowed");
    assert_eq!(refused["approvable"], false);
    assert!(
        !refused["message"]
            .as_str()
            .unwrap()
            .contains("agent approve"),
        "a hard refusal must not invite an approval: {refused}"
    );

    // And no approval can be armed for it.
    let armed = Command::new(sats_bin())
        .args(["agent", "approve", "k-r-1"])
        .env("SATS_DIR", dir.path())
        .env("SATS_PASSWORD", PASSWORD)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(!armed.status.success(), "strangers cannot be approved");
    assert!(String::from_utf8_lossy(&armed.stderr).contains("no approval can lift"));
    let retry = mcp.call_tool(4, "send", params);
    assert_eq!(retry["reason"], "recipient_not_allowed", "got: {retry}");

    // Allowing it (password-gated) makes it a routine ask; disallowing
    // (no password) flags it again.
    run_sats(&dir, &["agent", "allow", "claude", &stranger]);
    let grant = mcp.call_tool(6, "get_grant", serde_json::json!({}));
    let listed = grant["allowed_recipients"].as_array().unwrap();
    assert_eq!(listed.len(), 2, "got: {grant}");
    let now_routine = mcp.call_tool(
        7,
        "send",
        serde_json::json!({ "address": stranger, "amount_sat": 1_000, "request_id": "r-3" }),
    );
    assert_eq!(now_routine["reason"], "ask_required", "got: {now_routine}");
    run_sats(&dir, &["agent", "disallow", "claude", &stranger]);
    let asks = mcp.call_tool(
        8,
        "send",
        serde_json::json!({ "address": stranger, "amount_sat": 1_000, "request_id": "r-4" }),
    );
    assert_eq!(asks["reason"], "recipient_not_allowed", "got: {asks}");
}

/// Recorded truth outranks revocation, read-only: after the grant is
/// gone, a keyed retry of a send that signed still answers with the
/// recorded txid, and nothing new is written.
#[test]
fn revoked_keyed_retry_still_replays_recorded_truth() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--budget", "50000"], &[100_000]);
    arm_approval(&dir, "claude", ADDRESS, 1_000);

    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let params = serde_json::json!({
        "address": ADDRESS, "amount_sat": 1_000, "request_id": "job-1",
    });
    let sent = mcp.call_tool(2, "send", params.clone());
    assert_eq!(sent["status"], "sent", "got: {sent}");
    let txid = sent["txid"].as_str().unwrap().to_string();

    run_sats(&dir, &["agent", "revoke", "claude"]);

    let files_before: Vec<_> = std::fs::read_dir(dir.path().join("signet/agent-requests/claude"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let replay = daemon_begin_send(&dir, &fx.token, "job-1", 1_000);
    assert_eq!(replay["status"], "sent", "got: {replay}");
    assert_eq!(replay["txid"], txid.as_str());
    assert_eq!(replay["request_id"], "k-job-1");

    let files_after: Vec<_> = std::fs::read_dir(dir.path().join("signet/agent-requests/claude"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(files_before, files_after, "replay must not write");

    // A *fresh* key after revocation is denied and leaves no record.
    let denied = daemon_begin_send(&dir, &fx.token, "job-2", 1_000);
    assert_eq!(denied["reason"], "revoked");
    assert!(
        !dir.path()
            .join("signet/agent-requests/claude/k-job-2.json")
            .exists()
    );
}

#[test]
fn mcp_unlock_has_no_password_argument_and_rechecks_authority() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let token = grant_token(&dir, "claude", &["--budget", "50000"]);
    let mut mcp = McpSession::start(&dir, "claude", &token);
    handshake(&mut mcp);
    let missing = mcp.call_tool(2, "request_unlock", serde_json::json!({}));
    assert_eq!(missing["status"], "error");
    assert_eq!(missing["error_code"], "daemon_unavailable");
    assert!(missing.get("reason").is_none());
    let _daemon = Daemon::start(&dir);
    let unlocked = mcp.call_tool(3, "request_unlock", serde_json::json!({}));
    assert_eq!(unlocked["status"], "unlocked");
    assert!(!unlocked.to_string().contains(PASSWORD));
    mcp.send(serde_json::json!({"jsonrpc":"2.0", "id":4,"method":"tools/list"}));
    let list = mcp.recv();
    let tool = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "request_unlock")
        .unwrap();
    assert!(
        tool["inputSchema"]["properties"]
            .as_object()
            .is_none_or(|properties| properties.is_empty())
    );
    assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    mcp.send(serde_json::json!({"jsonrpc":"2.0", "id":5,"method":"tools/call", "params":{"name":"request_unlock", "arguments":{"password":"DO-NOT-ECHO-ME"}}}));
    let rejected = mcp.recv();
    assert!(
        rejected.get("error").is_some() || rejected["result"]["isError"] == true,
        "{rejected}"
    );
    assert!(!rejected.to_string().contains("DO-NOT-ECHO-ME"));
    run_sats(&dir, &["daemon", "lock"]);
    run_sats(&dir, &["agent", "revoke", "claude"]);
    let revoked = mcp.call_tool(6, "request_unlock", serde_json::json!({}));
    assert_eq!(revoked["error_code"], "unauthorized");
    let status = mcp.call_tool(7, "get_status", serde_json::json!({}));
    assert_eq!(status["daemon_state"], "locked");
}

#[test]
fn cancelling_mcp_unlock_or_closing_transport_closes_its_daemon_connection() {
    use std::os::unix::net::UnixListener;
    use std::time::Duration;
    for notification in [true, false] {
        let dir = TempDir::new().unwrap();
        run_sats(&dir, &["init"]);
        let token = grant_token(&dir, "claude", &["--budget", "50000"]);
        let listener = UnixListener::bind(dir.path().join("signet/d.sock")).unwrap();
        let (ready, wait) = std::sync::mpsc::channel();
        let expected_token = token.clone();
        let mock = std::thread::spawn(move || {
            let (mut probe, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(probe.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            writeln!(probe, "{}", serde_json::json!({"reply":"status", "protocol":1, "version":"test", "network":"signet", "locked":true,"grants":1,"locks_in":null})).unwrap();
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            line.clear();
            reader.read_line(&mut line).unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["op"], "request_unlock");
            assert_eq!(request["agent"], "claude");
            assert_eq!(request["token"], expected_token);
            assert!(request.get("password").is_none());
            ready.send(()).unwrap();
            line.clear();
            assert_eq!(
                reader.read_line(&mut line).unwrap(),
                0,
                "cancel must disconnect the helper's caller"
            );
        });
        let mut mcp = McpSession::start(&dir, "claude", &token);
        handshake(&mut mcp);
        mcp.send(serde_json::json!({"jsonrpc":"2.0", "id":2,"method":"tools/call", "params":{"name":"request_unlock", "arguments":{}}}));
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        if notification {
            mcp.send(serde_json::json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":2, "reason":"human cancelled"}}));
        } else {
            drop(mcp.child.stdin.take());
        }
        mock.join().unwrap();
    }
}

/// Opt-in desktop smoke. Uses only a disposable, unfunded signet wallet.
/// A human/test UI driver enters the public test password in the dialog.
#[test]
#[cfg(target_os = "macos")]
#[ignore = "opens a native macOS dialog; enter integration-test-pw in the isolated test wallet dialog"]
fn native_unlock_dialog() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let token = grant_token(&dir, "claude", &["--budget", "50000"]);
    let _daemon = Daemon::start(&dir);
    run_sats(&dir, &["daemon", "lock"]);
    let mut mcp = McpSession::start(&dir, "claude", &token);
    handshake(&mut mcp);
    let before = std::fs::read(dir.path().join("signet/grants/claude.json")).unwrap();
    eprintln!(
        "Native unlock smoke: disposable wallet {:?}; enter integration-test-pw",
        dir.path()
    );
    let result = mcp.call_tool(2, "request_unlock", serde_json::json!({}));
    assert_eq!(result["status"], "unlocked", "{result}");
    let status = mcp.call_tool(3, "get_status", serde_json::json!({}));
    assert_eq!(status["daemon_state"], "unlocked");
    assert_eq!(
        before,
        std::fs::read(dir.path().join("signet/grants/claude.json")).unwrap()
    );
    assert!(!dir.path().join("signet/transactions").exists());
    assert!(!result.to_string().contains(PASSWORD));
    assert!(!result.to_string().contains(&token));
    run_sats(&dir, &["daemon", "lock"]);
}
