//! `sats agent serve --http`: the same keyless tool surface over MCP
//! Streamable HTTP, as a remote chat client reaches it through a tunnel.
//! The agent files an frBTC wrap over HTTP, the human approves it with
//! the CLI, and the agent observes the result over HTTP.
#![cfg(feature = "mcp")]

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use common::mcp::*;
use tempfile::TempDir;

const PROTOCOL: &str = "2025-06-18";

/// A served `--http` process and one MCP session against it.
struct HttpSession {
    child: Child,
    /// `http://127.0.0.1:<port>`, as the server announced it.
    origin: String,
    token: String,
    session_id: Option<String>,
}

impl Drop for HttpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl HttpSession {
    fn start(dir: &TempDir, agent: &str, token: &str) -> HttpSession {
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
        let mut session = HttpSession {
            child,
            origin,
            token: token.to_string(),
            session_id: None,
        };
        session.handshake();
        session
    }

    fn post(&self, path: &str, body: &serde_json::Value) -> minreq::Response {
        let mut request = minreq::post(format!("{}{path}", self.origin))
            .with_header("Content-Type", "application/json")
            .with_header("Accept", "application/json, text/event-stream")
            .with_header("MCP-Protocol-Version", PROTOCOL)
            .with_body(body.to_string())
            .with_timeout(30);
        if let Some(id) = &self.session_id {
            request = request.with_header("Mcp-Session-Id", id);
        }
        request.send().unwrap()
    }

    fn endpoint(&self) -> String {
        format!("/mcp/{}", self.token)
    }

    /// One JSON-RPC request; the reply is the SSE event carrying its id.
    fn rpc(&mut self, id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
        let body = serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let response = self.post(&self.endpoint(), &body);
        assert_eq!(response.status_code, 200, "{}", response.as_str().unwrap());
        if let Some(session) = response.headers.get("mcp-session-id") {
            self.session_id = Some(session.clone());
        }
        response
            .as_str()
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str::<serde_json::Value>(data.trim()).ok())
            .find(|message| message["id"] == id)
            .unwrap_or_else(|| panic!("no reply to {id}: {}", response.as_str().unwrap()))
    }

    fn handshake(&mut self) {
        let init = self.rpc(
            1,
            "initialize",
            serde_json::json!({
                "protocolVersion": PROTOCOL,
                "capabilities": {},
                "clientInfo": {"name": "http-test", "version": "0"},
            }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "sats", "{init}");
        let note = serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert_eq!(self.post(&self.endpoint(), &note).status_code, 202);
    }

    fn call_tool(&mut self, id: u64, name: &str, args: serde_json::Value) -> serde_json::Value {
        let reply = self.rpc(
            id,
            "tools/call",
            serde_json::json!({"name": name, "arguments": args}),
        );
        reply["result"]["structuredContent"].clone()
    }
}

/// The mock chain and Subfrost answers an frBTC wrap needs, as in the
/// stdio end-to-end test.
fn wrap_ready_wallet(dir: &TempDir) -> String {
    run_sats(dir, &["init"]);
    let mock = common::write_mock_provider(dir);
    common::fund_wallet(dir, &[100_000]);
    std::fs::write(
        mock.join("alkanes-bytecode.json"),
        r#"{"32:0":"0x0061736d"}"#,
    )
    .unwrap();
    std::fs::write(
        mock.join("alkanes-signer.hex"),
        "0a241a22512030685674457c3eaa6d2904a1340ab80296625e8c7313f54c6ae7c51318f5860710e1fc02",
    )
    .unwrap();
    std::fs::write(mock.join("alkanes-wrap.hex"), "0a5e0a0d0a060a0208201200120308864e122b0a262f7365656e2fffdbcfe2faaab58dcfe6363d4cad67bf68ccae88d3e55e0a685a440e35a85f7412010112200a0c2f746f74616c737570706c7912107acbb22f0100000000000000000000001093880f").unwrap();
    grant_token(dir, "chatgpt", &["--max-tx", "10000"])
}

#[test]
fn a_wrap_filed_over_http_is_human_approved_and_observed_over_http() {
    let dir = TempDir::new().unwrap();
    let token = wrap_ready_wallet(&dir);
    let mut mcp = HttpSession::start(&dir, "chatgpt", &token);

    let tools = mcp.rpc(2, "tools/list", serde_json::json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"request_wrap_frbtc"), "{names:?}");
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
    let token = wrap_ready_wallet(&dir);
    let mut mcp = HttpSession::start(&dir, "chatgpt", &token);
    let init = serde_json::json!({"jsonrpc":"2.0","id":9,"method":"initialize","params":{
        "protocolVersion": PROTOCOL, "capabilities": {},
        "clientInfo": {"name": "stranger", "version": "0"}}});
    let wrong = format!("/mcp/{}", "0".repeat(token.len()));
    for path in ["/mcp", "/mcp/", "/", wrong.as_str()] {
        assert_eq!(mcp.post(path, &init).status_code, 404, "{path}");
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
    let token = wrap_ready_wallet(&dir);
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
