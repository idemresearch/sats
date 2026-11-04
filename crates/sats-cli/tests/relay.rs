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

/// Link the device and issue a relayed grant; returns the connection code
/// and the grant's bearer token.
fn relayed_grant(dir: &TempDir, relay: &str, agent: &str) -> (String, String) {
    let linked = sats_json(dir, &["--json", "relay", "link", relay]);
    assert_eq!(linked["url"], relay);
    let grant = sats_json(
        dir,
        &[
            "--json", "agent", "grant", agent, "--max-tx", "10000", "--relay",
        ],
    );
    assert_eq!(grant["relay_mcp_url"], format!("{relay}/mcp"));
    let code = grant["relay_code"].as_str().unwrap().to_string();
    // A bare code, never a URL: it is pasted on the sign-in page only.
    assert!(!code.contains('/') && !code.contains("http"), "{code}");
    // It names the grant but doesn't carry its token.
    let token = grant["token"].as_str().unwrap().to_string();
    use base64::Engine;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(code.split('.').next().unwrap())
        .unwrap();
    let payload = String::from_utf8(payload).unwrap();
    assert!(!payload.contains(&token), "{payload}");
    assert!(payload.contains(grant["grant_id"].as_str().unwrap()));
    (code, token)
}

/// Sign in as an OAuth client would, pasting `code`; returns an access token.
fn sign_in(relay: &str, code: &str) -> String {
    let redirect = "https://chatgpt.com/connector_platform_oauth_redirect";
    let registered: serde_json::Value = minreq::post(format!("{relay}/oauth/register"))
        .with_json(&serde_json::json!({"redirect_uris": [redirect]}))
        .unwrap()
        .send()
        .unwrap()
        .json()
        .unwrap();
    let client_id = registered["client_id"].as_str().unwrap();
    let verifier = "a-pkce-verifier-that-is-long-enough-to-satisfy-rfc-7636";
    let challenge = sats_relay::oauth::pkce_challenge(verifier);
    let (status, head, _) = raw(
        relay,
        "POST",
        "/oauth/authorize",
        "application/x-www-form-urlencoded",
        &form(&[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("code", code),
        ]),
    );
    assert_eq!(status, 302, "{head}");
    let auth_code = location_param(&head, "code").unwrap();
    let (status, _, body) = raw(
        relay,
        "POST",
        "/oauth/token",
        "application/x-www-form-urlencoded",
        &form(&[
            ("grant_type", "authorization_code"),
            ("code", &auth_code),
            ("redirect_uri", redirect),
            ("client_id", client_id),
            ("code_verifier", verifier),
        ]),
    );
    assert_eq!(status, 200, "{body}");
    let tokens: serde_json::Value = serde_json::from_str(&body).unwrap();
    tokens["access_token"].as_str().unwrap().to_string()
}

/// An MCP session on the relay, signed in with `code`.
fn connect_agent(relay: &str, code: &str) -> McpHttp {
    let token = sign_in(relay, code);
    McpHttp::connect_as(&format!("{relay}/mcp"), "sats-relay", Some(&token))
}

#[test]
fn a_wrap_filed_through_the_relay_is_decided_and_approved_on_the_device() {
    let relay_data = TempDir::new().unwrap();
    let relay = start_relay(&relay_data);
    let dir = TempDir::new().unwrap();
    wrap_ready_wallet(&dir);
    let (code, grant_token) = relayed_grant(&dir, &relay, "chatgpt");

    let mut agent = connect_agent(&relay, &code);
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
    // Approval posts the outcome to the relay itself: no sync needed.
    let sent = check(&mut agent, 9, &id);
    assert_eq!(sent["status"], "sent", "{sent}");
    assert!(sent["txid"].is_string());
    assert_eq!(saved_transactions(&dir), 1);
    // Terminal: the filing left the mailbox, the result stays observable.
    assert!(sync(&dir)["refreshed"].as_array().unwrap().is_empty());
    assert_eq!(wrap(&mut agent, 10, 10_000, "wrap-1")["status"], "sent");

    // At rest the relay holds no grant token, no connection code, no
    // readable filing, and no readable result: not the txid, the recipient,
    // or the operation.
    let db = std::fs::read(relay_data.path().join("relay.db")).unwrap();
    let txid = sent["txid"].as_str().unwrap().to_string();
    let recipient = sent["recipient"]
        .as_str()
        .unwrap_or("frbtc-signer")
        .to_string();
    for secret in [
        grant_token.as_str(),
        code.as_str(),
        "wrap-1",
        txid.as_str(),
        recipient.as_str(),
        "frbtc_wrap",
    ] {
        assert!(
            !db.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "relay store leaked {secret}"
        );
    }
}

#[test]
fn the_device_grant_decides_denials_and_revocation() {
    let relay_data = TempDir::new().unwrap();
    let relay = start_relay(&relay_data);
    let dir = TempDir::new().unwrap();
    wrap_ready_wallet(&dir);
    let (code, _) = relayed_grant(&dir, &relay, "chatgpt");
    let mut agent = connect_agent(&relay, &code);

    // A dismissal reaches the agent at once too.
    let declined = id_of(&wrap(&mut agent, 6, 1_000, "small"));
    sync(&dir);
    run_sats(&dir, &["agent", "dismiss", &declined]);
    assert_eq!(check(&mut agent, 7, &declined)["status"], "dismissed");

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
fn the_relay_admits_only_signed_in_agents_and_device_signed_codes() {
    let relay_data = TempDir::new().unwrap();
    let relay = start_relay(&relay_data);
    let dir = TempDir::new().unwrap();
    wrap_ready_wallet(&dir);
    let (code, _) = relayed_grant(&dir, &relay, "chatgpt");
    let init = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion": common::http::PROTOCOL, "capabilities": {},
        "clientInfo": {"name": "stranger", "version": "0"}}})
    .to_string();
    // The code is never a credential in a URL: those paths don't exist.
    for path in [
        format!("/mcp/{code}"),
        "/mcp/".to_string(),
        "/mcp/x.y".to_string(),
    ] {
        let (status, _, _) = raw(&relay, "POST", &path, "application/json", &init);
        assert_eq!(status, 404, "{path}");
    }
    // Without a token, /mcp points at OAuth.
    assert_eq!(
        raw(&relay, "POST", "/mcp", "application/json", &init).0,
        401
    );
    // A flipped character in the signed code: the sign-in page refuses it.
    let mut tampered = code.clone();
    let last = tampered.pop().unwrap();
    tampered.push(if last == 'A' { 'B' } else { 'A' });
    let redirect = "https://chatgpt.com/connector_platform_oauth_redirect";
    let registered: serde_json::Value = minreq::post(format!("{relay}/oauth/register"))
        .with_json(&serde_json::json!({"redirect_uris": [redirect]}))
        .unwrap()
        .send()
        .unwrap()
        .json()
        .unwrap();
    let challenge = sats_relay::oauth::pkce_challenge(
        "a-pkce-verifier-that-is-long-enough-to-satisfy-rfc-7636",
    );
    let (status, _, page) = raw(
        &relay,
        "POST",
        "/oauth/authorize",
        "application/x-www-form-urlencoded",
        &form(&[
            ("response_type", "code"),
            ("client_id", registered["client_id"].as_str().unwrap()),
            ("redirect_uri", redirect),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("code", &tampered),
        ]),
    );
    assert_eq!(status, 400);
    assert!(page.contains("valid connection code"));
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
fn raw_status(
    relay: &str,
    method: &str,
    path: &str,
    host: &str,
    bearer: Option<&str>,
    body: &str,
) -> String {
    use std::io::{Read, Write};
    let addr = relay.trim_start_matches("http://");
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\n{}Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        bearer
            .map(|token| format!("Authorization: Bearer {token}\r\n"))
            .unwrap_or_default(),
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
    let (code, _) = relayed_grant(&dir, &relay, "chatgpt");
    let token = sign_in(&relay, &code);
    let host = "sats-relay.up.example.app";
    assert!(raw_status(&relay, "GET", "/healthz", host, None, "").contains(" 200 "));
    let init = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion": common::http::PROTOCOL, "capabilities": {},
        "clientInfo": {"name": "proxied", "version": "0"}}})
    .to_string();
    assert!(raw_status(&relay, "POST", "/mcp", host, Some(&token), &init).contains(" 200 "));
    // Authentication, not the host, is what guards each route.
    assert!(raw_status(&relay, "POST", "/mcp", host, None, &init).contains(" 401 "));
    assert!(raw_status(&relay, "GET", "/v1/inbox", host, None, "").contains(" 401 "));
}

/// One raw HTTP/1.1 exchange, so redirects are seen rather than followed.
/// Returns (status code, headers, body).
fn raw(
    relay: &str,
    method: &str,
    path: &str,
    content_type: &str,
    body: &str,
) -> (u16, String, String) {
    use std::io::{Read, Write};
    let addr = relay.trim_start_matches("http://");
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, head.to_string(), body.to_string())
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", sats_relay::oauth::encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn location_param(head: &str, name: &str) -> Option<String> {
    let location = head.lines().find_map(|line| {
        line.strip_prefix("location: ")
            .or_else(|| line.strip_prefix("Location: "))
    })?;
    let query = location.split_once('?')?.1;
    sats_relay::oauth::parse_form(query)
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

/// The OAuth flow as ChatGPT runs it: discover, register, sign in with the
/// connection code, trade the code for tokens with PKCE, then file.
#[test]
fn an_agent_signs_in_with_oauth_and_files_with_a_bearer_token() {
    let relay_data = TempDir::new().unwrap();
    let relay = start_relay(&relay_data);
    let dir = TempDir::new().unwrap();
    wrap_ready_wallet(&dir);
    let grant = sats_json(&dir, &["--json", "relay", "link", &relay]);
    assert_eq!(grant["url"], relay);
    let grant = sats_json(
        &dir,
        &[
            "--json", "agent", "grant", "chatgpt", "--max-tx", "10000", "--relay",
        ],
    );
    let code_text = grant["relay_code"].as_str().unwrap().to_string();
    assert_eq!(grant["relay_mcp_url"], format!("{relay}/mcp"));

    // 1. An unauthenticated call points at the metadata.
    let (status, head, _) = raw(&relay, "POST", "/mcp", "application/json", "{}");
    assert_eq!(status, 401);
    assert!(
        head.to_lowercase().contains(&format!(
            "www-authenticate: bearer resource_metadata=\"{relay}/.well-known/oauth-protected-resource\""
        )),
        "{head}"
    );
    // 2-3. Discovery.
    let resource: serde_json::Value =
        minreq::get(format!("{relay}/.well-known/oauth-protected-resource"))
            .send()
            .unwrap()
            .json()
            .unwrap();
    assert_eq!(resource["resource"], format!("{relay}/mcp"));
    assert_eq!(resource["authorization_servers"][0], relay);
    let server: serde_json::Value =
        minreq::get(format!("{relay}/.well-known/oauth-authorization-server"))
            .send()
            .unwrap()
            .json()
            .unwrap();
    assert_eq!(
        server["code_challenge_methods_supported"],
        serde_json::json!(["S256"])
    );
    // 4. Dynamic client registration.
    let redirect = "https://chatgpt.com/connector_platform_oauth_redirect";
    let registered: serde_json::Value =
        minreq::post(server["registration_endpoint"].as_str().unwrap())
            .with_json(&serde_json::json!({"redirect_uris": [redirect], "client_name": "ChatGPT"}))
            .unwrap()
            .send()
            .unwrap()
            .json()
            .unwrap();
    let client_id = registered["client_id"].as_str().unwrap().to_string();
    let (status, _, _) = raw(
        &relay,
        "POST",
        "/oauth/register",
        "application/json",
        r#"{"redirect_uris":["http://evil.example/cb"]}"#,
    );
    assert_eq!(status, 400);

    // 5. The sign-in page, then the human pastes the connection code.
    let verifier = "a-pkce-verifier-that-is-long-enough-to-satisfy-rfc-7636";
    let challenge = sats_relay::oauth::pkce_challenge(verifier);
    let params = |extra: &[(&str, &str)]| {
        let mut pairs = vec![
            ("response_type", "code"),
            ("client_id", client_id.as_str()),
            ("redirect_uri", redirect),
            ("state", "xyz"),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ];
        pairs.extend_from_slice(extra);
        form(&pairs)
    };
    let (status, head, page) = raw(
        &relay,
        "GET",
        &format!("/oauth/authorize?{}", params(&[])),
        "text/plain",
        "",
    );
    assert_eq!(status, 200);
    assert!(
        page.contains("chatgpt.com") && page.contains("sats agent grant"),
        "{page}"
    );
    assert!(head.to_lowercase().contains("x-frame-options: deny"));
    let wrong = raw(
        &relay,
        "GET",
        &format!(
            "/oauth/authorize?{}",
            form(&[
                ("response_type", "code"),
                ("client_id", &client_id),
                ("redirect_uri", "https://evil.example/cb"),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
            ])
        ),
        "text/plain",
        "",
    );
    assert_eq!(wrong.0, 400, "an unregistered redirect never gets a form");
    let (status, _, page) = raw(
        &relay,
        "POST",
        "/oauth/authorize",
        "application/x-www-form-urlencoded",
        &params(&[("code", "not-a-code")]),
    );
    assert_eq!(status, 400);
    assert!(page.contains("valid connection code"));
    let authorize = |relay: &str| {
        let (status, head, _) = raw(
            relay,
            "POST",
            "/oauth/authorize",
            "application/x-www-form-urlencoded",
            &params(&[("code", &code_text)]),
        );
        assert_eq!(status, 302, "{head}");
        assert_eq!(location_param(&head, "state").as_deref(), Some("xyz"));
        location_param(&head, "code").unwrap()
    };

    // 6. Code for tokens: PKCE must match, and a code works once.
    let token = |pairs: &[(&str, &str)]| {
        let (status, _, body) = raw(
            &relay,
            "POST",
            "/oauth/token",
            "application/x-www-form-urlencoded",
            &form(pairs),
        );
        (
            status,
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        )
    };
    let first = authorize(&relay);
    let wrong_verifier = "another-pkce-verifier-that-is-long-enough-for-rfc-7636";
    let (status, body) = token(&[
        ("grant_type", "authorization_code"),
        ("code", &first),
        ("redirect_uri", redirect),
        ("client_id", &client_id),
        ("code_verifier", wrong_verifier),
    ]);
    assert_eq!(
        (status, body["error"].as_str()),
        (400, Some("invalid_grant"))
    );
    let (status, _) = token(&[
        ("grant_type", "authorization_code"),
        ("code", &first),
        ("redirect_uri", redirect),
        ("client_id", &client_id),
        ("code_verifier", verifier),
    ]);
    assert_eq!(status, 400, "a code is spent by its first use");
    let code = authorize(&relay);
    let (status, tokens) = token(&[
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("redirect_uri", redirect),
        ("client_id", &client_id),
        ("code_verifier", verifier),
    ]);
    assert_eq!(status, 200, "{tokens}");
    assert_eq!(tokens["token_type"], "Bearer");
    let access = tokens["access_token"].as_str().unwrap().to_string();
    let refresh = tokens["refresh_token"].as_str().unwrap().to_string();

    // 7. File and observe with the bearer token on the plain /mcp URL.
    let mut agent = McpHttp::connect_as(&format!("{relay}/mcp"), "sats-relay", Some(&access));
    let filed = wrap(&mut agent, 2, 10_000, "oauth-wrap");
    assert_eq!(filed["status"], "pending_approval", "{filed}");
    let id = id_of(&filed);
    assert_eq!(sync(&dir)["created"], serde_json::json!([id]));
    assert_eq!(check(&mut agent, 3, &id)["operation"], "frbtc_wrap");
    let (status, _, _) = raw(&relay, "POST", "/mcp", "application/json", "{}");
    assert_eq!(status, 401);

    // 8. Refresh rotates: the old refresh token is spent.
    let (status, rotated) = token(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", &refresh),
        ("client_id", &client_id),
    ]);
    assert_eq!(status, 200, "{rotated}");
    assert_ne!(rotated["refresh_token"], refresh.as_str());
    let (status, _) = token(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", &refresh),
        ("client_id", &client_id),
    ]);
    assert_eq!(status, 400);
    let mut renewed = McpHttp::connect_as(
        &format!("{relay}/mcp"),
        "sats-relay",
        rotated["access_token"].as_str(),
    );
    assert_eq!(check(&mut renewed, 2, &id)["request_id"], id);

    // At rest, the relay holds no token, code, capability, or grant token.
    let db = std::fs::read(relay_data.path().join("relay.db")).unwrap();
    let capability = code_text.clone();
    for secret in [&access, &refresh, &capability] {
        assert!(
            !db.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "relay store leaked a credential"
        );
    }
}
