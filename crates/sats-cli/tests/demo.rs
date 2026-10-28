//! The reproducible MCP demo: one agent session per scene, driven over
//! the served process's stdio exactly as an MCP client would, with the
//! human's side through the CLI. Offline and deterministic: an isolated
//! SATS_DIR per scene, the file-driven mock chain, and a directly funded
//! signet wallet.
//!
//! Each scene prints a transcript and asserts on it, so the demo is also
//! a test. Run it with `sh scripts/demo-mcp.sh`.
#![cfg(feature = "mcp")]

mod common;

use common::mcp::*;
use tempfile::TempDir;

const AMOUNT: u64 = 10_000;

fn scene(title: &str) {
    println!("\n━━ {title}");
}

fn agent(tool: &str, result: &serde_json::Value) {
    let shown: serde_json::Map<_, _> = result
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| ["status", "reason", "error_code", "txid"].contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    println!("  agent  {tool:<14} → {}", serde_json::Value::Object(shown));
}

fn human(action: &str, outcome: &str) {
    println!("  human  {action:<14} → {outcome}");
}

/// A funded wallet, a granted agent, and its live MCP session with one
/// pending request filed.
fn filed(grant_args: &[&str]) -> (TempDir, Fixture, McpSession, String) {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, grant_args, &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let view = mcp.request_send(2, AMOUNT, "invoice-42");
    agent("request_send", &view);
    assert_eq!(view["status"], "pending_approval", "{view}");
    let id = id_of(&view);
    (dir, fx, mcp, id)
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}

#[test]
fn mcp_demo() {
    println!("sats MCP demo: agents create requests, humans authorize them, sats executes.");

    scene("1. Approval: the agent asks, the human approves, sats signs and broadcasts");
    let (dir, _fx, mut mcp, id) = filed(&[]);
    let approved = approve(&dir, &id);
    human("agent approve", approved["status"].as_str().unwrap());
    assert_eq!(approved["status"], "sent", "{approved}");
    let view = mcp.check_request(3, &id);
    agent("check_request", &view);
    assert_eq!(view["status"], "sent");
    assert_eq!(view["txid"], approved["txid"]);

    scene("2. Denial: a proposal over the grant's cap is never approvable");
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--max-tx", "5000"], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let view = mcp.request_send(2, AMOUNT, "too-big");
    agent("request_send", &view);
    assert_eq!(view["status"], "denied", "{view}");
    assert_eq!(view["reason"], "over_max_tx");
    let output = sats_output(&dir, &["agent", "approve", &id_of(&view), "--yes"]);
    human("agent approve", &stderr_of(&output));
    assert!(!output.status.success());
    assert_eq!(saved_transactions(&dir), 0);

    scene("3. Expiry: a request pending when its grant expires is denied at approval");
    let (dir, _fx, mut mcp, id) = filed(&[]);
    let grant_path = dir.path().join("signet/grants/claude.json");
    let mut grant = read_json(&grant_path);
    grant["expires_at"] = serde_json::json!(1);
    std::fs::write(&grant_path, grant.to_string()).unwrap();
    human("(time passes)", "grant expired");
    let view = mcp.check_request(3, &id);
    agent("check_request", &view);
    assert_eq!(view["error_code"], "no_grant");
    let output = sats_output(&dir, &["agent", "approve", &id, "--yes"]);
    human("agent approve", &stderr_of(&output));
    assert!(!output.status.success());
    assert!(stderr_of(&output).contains("expired"));
    assert_eq!(request_record(&dir, "claude", &id)["status"], "denied");
    assert_eq!(saved_transactions(&dir), 0);

    scene("4. Revocation: the human revokes mid-session; every tool is cut off");
    let (dir, _fx, mut mcp, id) = filed(&[]);
    run_sats(&dir, &["agent", "revoke", "claude"]);
    human("agent revoke", "revoked");
    let view = mcp.request_send(3, AMOUNT, "after-revoke");
    agent("request_send", &view);
    assert_eq!(view["error_code"], "no_grant");
    let output = sats_output(&dir, &["agent", "approve", &id, "--yes"]);
    human("agent approve", &stderr_of(&output));
    assert!(!output.status.success());
    assert!(stderr_of(&output).contains("revoked"));
    assert_eq!(request_record(&dir, "claude", &id)["status"], "denied");

    scene("5. Failed broadcast: the signed transaction is kept and never signed again");
    let (dir, fx, mut mcp, id) = filed(&[]);
    std::fs::write(fx.mockdata.join("broadcast-fail"), "provider down").unwrap();
    let approved = approve(&dir, &id);
    human("agent approve", approved["status"].as_str().unwrap());
    assert_eq!(approved["status"], "broadcast_pending", "{approved}");
    let txid = approved["txid"].as_str().unwrap().to_string();
    let view = mcp.check_request(3, &id);
    agent("check_request", &view);
    assert_eq!(view["status"], "broadcast_pending");
    std::fs::remove_file(fx.mockdata.join("broadcast-fail")).unwrap();
    run_sats(&dir, &["tx", "broadcast", &txid]);
    human("tx broadcast", "sent");
    let view = mcp.check_request(4, &id);
    agent("check_request", &view);
    assert_eq!(view["status"], "sent");
    assert_eq!(view["txid"], txid.as_str());
    assert_eq!(saved_transactions(&dir), 1, "one signature");

    println!("\nAll five scenes behaved as specified.");
}
