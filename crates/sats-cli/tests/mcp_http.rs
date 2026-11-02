//! `sats agent serve --http`: the same keyless tool surface over MCP
//! Streamable HTTP, as a remote chat client reaches it through a tunnel.
//! The agent files an frBTC wrap over HTTP, the human approves it with
//! the CLI, and the agent observes the result over HTTP.
#![cfg(feature = "mcp")]

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use common::http::{McpHttp, PROTOCOL, wrap_ready_grant};
use common::mcp::*;
use tempfile::TempDir;

/// A served `--http` process. Killed on drop.
struct Served {
    child: Child,
    /// `http://127.0.0.1:<port>`, as the server announced it.
    origin: String,
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve(dir: &TempDir, agent: &str, token: &str) -> Served {
    let mut child = Command::new(sats_bin())
        .args(["agent", "serve", agent, "--http", "127.0.0.1:0"])
        .env("SATS_DIR", dir.path())
        .env("SATS_AGENT_TOKEN", token)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut log = String::new();
    let origin = loop {
        let mut line = String::new();
        assert!(
            stderr.read_line(&mut line).unwrap() > 0,
            "server exited before listening: {log}"
        );
        log.push_str(&line);
        if let Some(rest) = line.trim().split("http://").nth(1) {
            // The announcement never carries the token itself.
            assert!(!line.contains(token), "{line}");
            break format!("http://{}", rest.split("/mcp/").next().unwrap());
        }
    };
    Served { child, origin }
}

#[test]
fn a_wrap_filed_over_http_is_human_approved_and_observed_over_http() {
    let dir = TempDir::new().unwrap();
    let token = wrap_ready_grant(&dir, "chatgpt");
    let served = serve(&dir, "chatgpt", &token);
    let mut mcp = McpHttp::connect(&format!("{}/mcp/{token}", served.origin), "sats");

    let names = mcp.tool_names(2);
    assert!(names.iter().any(|n| n == "request_wrap_frbtc"), "{names:?}");
    for forbidden in ["approve", "sign", "broadcast", "unlock"] {
        assert!(
            !names.iter().any(|name| name.contains(forbidden)),
            "{names:?}"
        );
    }

    let args = serde_json::json!({"amount_sat": 10000, "idempotency_key": "wrap-phone-1"});
    let filed = mcp.call_tool(3, "request_wrap_frbtc", args.clone());
    assert_eq!(filed["status"], "pending_approval", "{filed}");
    assert_eq!(filed["operation"], "frbtc_wrap");
    let id = id_of(&filed);
    // A retried call over HTTP returns the same request.
    assert_eq!(
        mcp.call_tool(4, "request_wrap_frbtc", args)["request_id"],
        id
    );

    let approval = sats_output(&dir, &["--json", "agent", "approve", &id, "--yes"]);
    assert!(
        approval.status.success(),
        "{}",
        String::from_utf8_lossy(&approval.stderr)
    );
    let observed = mcp.call_tool(5, "check_request", serde_json::json!({"request_id": id}));
    assert_eq!(observed["status"], "sent", "{observed}");
    assert_eq!(saved_transactions(&dir), 1);
}

#[test]
fn the_http_endpoint_needs_the_token_path_and_honours_revocation() {
    let dir = TempDir::new().unwrap();
    let token = wrap_ready_grant(&dir, "chatgpt");
    let served = serve(&dir, "chatgpt", &token);
    let mut mcp = McpHttp::connect(&format!("{}/mcp/{token}", served.origin), "sats");
    let init = serde_json::json!({"jsonrpc":"2.0","id":9,"method":"initialize","params":{
        "protocolVersion": PROTOCOL, "capabilities": {},
        "clientInfo": {"name": "stranger", "version": "0"}}});
    let wrong = format!("/mcp/{}", "0".repeat(token.len()));
    for path in ["/mcp", "/mcp/", "/", wrong.as_str()] {
        let url = format!("{}{path}", served.origin);
        assert_eq!(mcp.post_to(&url, &init).status_code, 404, "{path}");
    }
    assert_eq!(
        mcp.call_tool(2, "get_grant", serde_json::json!({}))["active"],
        true
    );

    run_sats(&dir, &["agent", "revoke", "chatgpt"]);
    // The connected session is cut off at its next call.
    assert_eq!(
        mcp.call_tool(3, "get_grant", serde_json::json!({}))["active"],
        false
    );
}

#[test]
fn http_refuses_a_non_loopback_address() {
    let dir = TempDir::new().unwrap();
    let token = wrap_ready_grant(&dir, "chatgpt");
    let output = Command::new(sats_bin())
        .args(["agent", "serve", "chatgpt", "--http", "0.0.0.0:0"])
        .env("SATS_DIR", dir.path())
        .env("SATS_AGENT_TOKEN", &token)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("loopback"), "{stderr}");
}
