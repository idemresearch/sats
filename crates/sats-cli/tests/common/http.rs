//! An MCP Streamable HTTP client for the tests, as a remote agent (a chat
//! app connecting from its provider's servers) would drive one.

use tempfile::TempDir;

use super::mcp::grant_token;
use super::mcp::run_sats;

pub const PROTOCOL: &str = "2025-06-18";

/// One MCP session against an HTTP endpoint.
pub struct McpHttp {
    /// The full endpoint URL, credential included.
    pub url: String,
    session_id: Option<String>,
    /// An OAuth access token, sent as `Authorization: Bearer`.
    bearer: Option<String>,
}

impl McpHttp {
    /// Initialize a session; `server` is the name the server must report.
    pub fn connect(url: &str, server: &str) -> McpHttp {
        McpHttp::connect_as(url, server, None)
    }

    /// [`McpHttp::connect`] with an OAuth access token.
    pub fn connect_as(url: &str, server: &str, bearer: Option<&str>) -> McpHttp {
        let mut session = McpHttp {
            url: url.to_string(),
            session_id: None,
            bearer: bearer.map(str::to_string),
        };
        let init = session.rpc(
            1,
            "initialize",
            serde_json::json!({
                "protocolVersion": PROTOCOL,
                "capabilities": {},
                "clientInfo": {"name": "http-test", "version": "0"},
            }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], server, "{init}");
        let note = serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert_eq!(session.post_to(url, &note).status_code, 202);
        session
    }

    pub fn post_to(&self, url: &str, body: &serde_json::Value) -> minreq::Response {
        let mut request = minreq::post(url)
            .with_header("Content-Type", "application/json")
            .with_header("Accept", "application/json, text/event-stream")
            .with_header("MCP-Protocol-Version", PROTOCOL)
            .with_body(body.to_string())
            .with_timeout(30);
        if let Some(id) = &self.session_id {
            request = request.with_header("Mcp-Session-Id", id);
        }
        if let Some(token) = &self.bearer {
            request = request.with_header("Authorization", format!("Bearer {token}"));
        }
        request.send().unwrap()
    }

    /// One JSON-RPC request; the reply is the SSE event carrying its id.
    pub fn rpc(&mut self, id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
        let body = serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let response = self.post_to(&self.url, &body);
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

    pub fn call_tool(&mut self, id: u64, name: &str, args: serde_json::Value) -> serde_json::Value {
        let reply = self.rpc(
            id,
            "tools/call",
            serde_json::json!({"name": name, "arguments": args}),
        );
        reply["result"]["structuredContent"].clone()
    }

    pub fn tool_names(&mut self, id: u64) -> Vec<String> {
        let tools = self.rpc(id, "tools/list", serde_json::json!({}));
        tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect()
    }
}

/// A funded wallet with the mock chain and Subfrost answers an frBTC wrap
/// needs, as in the stdio end-to-end test. Returns nothing granted yet.
pub fn wrap_ready_wallet(dir: &TempDir) {
    run_sats(dir, &["init"]);
    let mock = super::write_mock_provider(dir);
    super::fund_wallet(dir, &[100_000]);
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
}

/// [`wrap_ready_wallet`] plus an ask grant for `agent` capped at 10k sat.
pub fn wrap_ready_grant(dir: &TempDir, agent: &str) -> String {
    wrap_ready_wallet(dir);
    grant_token(dir, agent, &["--max-tx", "10000"])
}
