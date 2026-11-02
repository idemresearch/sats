//! sats agent → sats relay → sats human, end to end.
//!
//! A remote agent (ChatGPT on a phone, say) reaches the wallet only
//! through a relay. The relay is the real `sats_relay` server on a
//! loopback port; the device is the `sats` CLI linked to it. The agent
//! files a wrap, the device syncs and decides it against its own grant,
//! the human approves with the CLI, and the agent observes `sent`
//! through the relay. The relay never holds a key, and every policy
//! decision is the device's.
#![cfg(feature = "mcp")]

mod common;

use common::http::{McpHttp, wrap_ready_grant, wrap_ready_wallet};
use common::mcp::*;
use tempfile::TempDir;

/// A relay on a loopback port, alive for the test process.
fn start_relay(data: &TempDir) -> String {
    let config = sats_relay::Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: data.path().to_path_buf(),
        filing_ttl: 3600,
        result_ttl: 3600,
    };
    let (ready, bound) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind(config.listen).await.unwrap();
            ready.send(listener.local_addr().unwrap()).unwrap();
            sats_relay::serve(listener, config).await.unwrap();
        });
    });
    format!("http://{}", bound.recv().unwrap())
}

fn sync(dir: &TempDir) -> serde_json::Value {
    sats_json(dir, &["--json", "relay", "sync"])
}

fn wrap(mcp: &mut McpHttp, id: u64, amount: u64, key: &str) -> serde_json::Value {
    mcp.call_tool(
        id,
        "request_wrap_frbtc",
        serde_json::json!({"amount_sat": amount, "idempotency_key": key}),
    )
}

fn check(mcp: &mut McpHttp, id: u64, request_id: &str) -> serde_json::Value {
    mcp.call_tool(
        id,
        "check_request",
        serde_json::json!({"request_id": request_id}),
    )
}

/// Link the device and issue a relayed grant; returns the agent's URL.
fn relayed_grant(dir: &TempDir, relay: &str, agent: &str) -> String {
    let linked = sats_json(dir, &["--json", "relay", "link", relay]);
    assert_eq!(linked["url"], relay);
    let grant = sats_json(
        dir,
        &[
            "--json", "agent", "grant", agent, "--max-tx", "10000", "--relay",
        ],
    );
    let url = grant["relay_url"].as_str().unwrap().to_string();
    assert!(url.starts_with(&format!("{relay}/mcp/")), "{url}");
    url
}

#[test]
fn a_wrap_filed_through_the_relay_is_decided_and_approved_on_the_device() {
    let relay_data = TempDir::new().unwrap();
    let relay = start_relay(&relay_data);
    let dir = TempDir::new().unwrap();
    wrap_ready_wallet(&dir);
    let url = relayed_grant(&dir, &relay, "chatgpt");

    let mut agent = McpHttp::connect(&url, "sats-relay");
    let names = agent.tool_names(2);
    for expected in [
        "request_send",
        "request_wrap_frbtc",
        "check_request",
        "get_grant",
    ] {
        assert!(names.iter().any(|n| n == expected), "{names:?}");
    }
    for forbidden in ["approve", "sign", "broadcast", "unlock"] {
        assert!(!names.iter().any(|n| n.contains(forbidden)), "{names:?}");
    }
    assert_eq!(
        agent.call_tool(3, "get_grant", serde_json::json!({}))["active"],
        true
    );

    // Filed at the relay: queued, nothing on the device yet.
    let filed = wrap(&mut agent, 4, 10_000, "wrap-1");
    assert_eq!(filed["status"], "pending_approval", "{filed}");
    let id = id_of(&filed);
    assert_eq!(wrap(&mut agent, 5, 10_000, "wrap-1")["request_id"], id);
    assert_eq!(
        wrap(&mut agent, 6, 9_000, "wrap-1")["error_code"],
        "idempotency_key_conflict"
    );
    assert!(
        sats_json(&dir, &["--json", "agent", "requests"])
            .as_array()
            .unwrap()
            .is_empty()
    );

    // The device syncs: the filing goes through its own request workflow.
    let report = sync(&dir);
    assert_eq!(report["created"], serde_json::json!([id]), "{report}");
    let observed = check(&mut agent, 7, &id);
    assert_eq!(observed["status"], "pending_approval", "{observed}");
    assert_eq!(observed["operation"], "frbtc_wrap");
    assert!(observed["frbtc"]["code_hash"].is_string(), "{observed}");
    // A second sync files nothing twice.
    assert_eq!(sync(&dir)["created"], serde_json::json!([]));

    // The human approves here, as for a local agent; the agent sees sent.
    let approval = sats_output(&dir, &["--json", "agent", "approve", &id, "--yes"]);
    assert!(
        approval.status.success(),
        "{}",
        String::from_utf8_lossy(&approval.stderr)
    );
    assert_eq!(check(&mut agent, 8, &id)["status"], "pending_approval");
    sync(&dir);
    let sent = check(&mut agent, 9, &id);
    assert_eq!(sent["status"], "sent", "{sent}");
    assert!(sent["txid"].is_string());
    assert_eq!(saved_transactions(&dir), 1);
    // Terminal: the filing left the mailbox, the result stays observable.
    assert!(sync(&dir)["refreshed"].as_array().unwrap().is_empty());
    assert_eq!(wrap(&mut agent, 10, 10_000, "wrap-1")["status"], "sent");

    // The relay's store holds no token and no plaintext filing.
    let db = std::fs::read(relay_data.path().join("relay.db")).unwrap();
    let grant_token = url.rsplit('/').next().unwrap();
    for secret in [grant_token.as_bytes(), b"wrap-1".as_slice()] {
        assert!(
            !db.windows(secret.len()).any(|w| w == secret),
            "relay store leaked plaintext"
        );
    }
}

#[test]
fn the_device_grant_decides_denials_and_revocation() {
    let relay_data = TempDir::new().unwrap();
    let relay = start_relay(&relay_data);
    let dir = TempDir::new().unwrap();
    wrap_ready_wallet(&dir);
    let url = relayed_grant(&dir, &relay, "chatgpt");
    let mut agent = McpHttp::connect(&url, "sats-relay");

    // Over the device's 10k cap: the relay queues it, the device denies it.
    let over = id_of(&wrap(&mut agent, 2, 20_000, "big"));
    sync(&dir);
    let denied = check(&mut agent, 3, &over);
    assert_eq!(denied["status"], "denied", "{denied}");
    assert_eq!(denied["reason"], "over_max_tx");

    // Revoked on the device: the next filing is refused there, not run.
    run_sats(&dir, &["agent", "revoke", "chatgpt"]);
    let late = id_of(&wrap(&mut agent, 4, 1_000, "late"));
    let report = sync(&dir);
    assert_eq!(report["refused"][0]["request_id"], late, "{report}");
    let refused = check(&mut agent, 5, &late);
    assert_eq!(refused["status"], "error", "{refused}");
    assert_eq!(refused["error_code"], "no_grant");
    assert!(
        sats_json(&dir, &["--json", "agent", "requests", "--all"])
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["id"] != late)
    );
}

#[test]
fn the_relay_routes_only_device_signed_capabilities() {
    let relay_data = TempDir::new().unwrap();
    let relay = start_relay(&relay_data);
    let dir = TempDir::new().unwrap();
    wrap_ready_wallet(&dir);
    let url = relayed_grant(&dir, &relay, "chatgpt");
    let agent = McpHttp::connect(&url, "sats-relay");
    let init = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion": common::http::PROTOCOL, "capabilities": {},
        "clientInfo": {"name": "stranger", "version": "0"}}});
    // A flipped character in the signed capability: an empty 404.
    let mut tampered = url.clone();
    let last = tampered.pop().unwrap();
    tampered.push(if last == 'A' { 'B' } else { 'A' });
    for bad in [
        tampered,
        format!("{relay}/mcp/"),
        format!("{relay}/mcp/x.y"),
    ] {
        assert_eq!(agent.post_to(&bad, &init).status_code, 404, "{bad}");
    }
    // Device endpoints refuse unsigned calls.
    let unsigned = minreq::get(format!("{relay}/v1/inbox")).send().unwrap();
    assert_eq!(unsigned.status_code, 401);
    assert_eq!(
        minreq::get(format!("{relay}/healthz"))
            .send()
            .unwrap()
            .status_code,
        200
    );
}

#[test]
fn a_relayed_grant_needs_a_linked_relay_reached_over_https() {
    let dir = TempDir::new().unwrap();
    let _ = wrap_ready_grant(&dir, "local");
    let output = sats_output(&dir, &["agent", "grant", "chatgpt", "--relay"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("sats relay link"), "{stderr}");
    let output = sats_output(&dir, &["relay", "link", "http://relay.example.com"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("https"));
}

/// Send one raw HTTP/1.1 request with a chosen Host header, as a proxy in
/// front of the relay would; returns the status line.
fn raw_status(relay: &str, method: &str, path: &str, host: &str, body: &str) -> String {
    use std::io::{Read, Write};
    let addr = relay.trim_start_matches("http://");
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response.lines().next().unwrap_or_default().to_string()
}

#[test]
fn the_relay_serves_whatever_host_a_proxy_forwards() {
    let relay_data = TempDir::new().unwrap();
    let relay = start_relay(&relay_data);
    let dir = TempDir::new().unwrap();
    wrap_ready_wallet(&dir);
    let url = relayed_grant(&dir, &relay, "chatgpt");
    let path = &url[relay.len()..];
    let host = "sats-relay.up.example.app";
    assert!(raw_status(&relay, "GET", "/healthz", host, "").contains(" 200 "));
    let init = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion": common::http::PROTOCOL, "capabilities": {},
        "clientInfo": {"name": "proxied", "version": "0"}}})
    .to_string();
    assert!(raw_status(&relay, "POST", path, host, &init).contains(" 200 "));
    // Authentication, not the host, is what guards each route.
    assert!(raw_status(&relay, "GET", "/v1/inbox", host, "").contains(" 401 "));
}
