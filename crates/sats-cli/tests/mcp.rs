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

use std::process::{Command, Stdio};

use common::mcp::*;
use tempfile::TempDir;

fn assert_alkanes_error(response: &serde_json::Value, code: &str) {
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert_eq!(
        response["result"]["structuredContent"]["status"], "error",
        "{response}"
    );
    assert_eq!(
        response["result"]["structuredContent"]["error_code"], code,
        "{response}"
    );
}

fn assert_alkanes_reads_refused(mcp: &mut McpSession, code: &str) {
    for (index, (tool, args)) in [
        ("alkanes_balance", serde_json::json!({})),
        ("alkanes_inspect", serde_json::json!({"id": "32:0"})),
        (
            "alkanes_simulate",
            serde_json::json!({"id": "32:0", "inputs": ["77"]}),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let response = mcp.call_tool_raw(80 + index as u64, tool, args);
        assert_alkanes_error(&response, code);
    }
}

#[test]
fn alkanes_reads_preserve_precision_unknown_results_and_the_pinned_network_without_keys() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let mock = common::write_mock_provider(&dir);
    let funded = common::fund_wallet(&dir, &[100_000, 20_000]);
    let max = u128::MAX.to_string();
    std::fs::write(
        mock.join("alkanes-outpoints.json"),
        serde_json::json!({
            funded[0].to_string(): {"32:0": max},
            funded[1].to_string(): null,
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        mock.join("alkanes-bytecode.json"),
        r#"{"32:0":"0x0061736d"}"#,
    )
    .unwrap();
    let token = grant_token(&dir, "claude", &["--mode", "observe"]);
    let grant_path = dir.path().join("signet/grants/claude.json");
    let grant_before = std::fs::read(&grant_path).unwrap();
    let mut mcp = McpSession::start(&dir, "claude", &token);
    handshake(&mut mcp);

    // Reads cannot depend on a seed. Changing the CLI default cannot move
    // an existing connection or its grant to another network.
    std::fs::remove_file(dir.path().join("seed.sealed")).unwrap();
    let config_path = dir.path().join("config.toml");
    let config = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(
        config_path,
        config.replace("network = \"signet\"", "network = \"testnet4\""),
    )
    .unwrap();
    let balance = mcp.call_tool(2, "alkanes_balance", serde_json::json!({}));
    assert_eq!(balance["network"], "signet", "{balance}");
    assert_eq!(balance["assets"][0]["id"], "32:0");
    assert_eq!(balance["assets"][0]["amount"], max);
    assert_eq!(balance["outputs"][0]["value_sat"], 100_000);
    assert_eq!(balance["outputs"][0]["balances"][0]["amount"], max);
    assert_eq!(
        balance["unindexed_outputs"],
        serde_json::json!([funded[1].to_string()])
    );

    let inspected = mcp.call_tool(3, "alkanes_inspect", serde_json::json!({"id": "32:0"}));
    assert_eq!(inspected["network"], "signet");
    assert_eq!(inspected["id"], "32:0");
    assert_eq!(inspected["bytecode_bytes"], 4);
    assert_eq!(
        inspected["code_hash"],
        "cd5d4935a48c0672cb06407bb443bc0087aff947c6b864bac886982c73b3027f"
    );

    for (index, raw) in [
        serde_json::json!({"status":0,"gasUsed":123,"execution":{"alkanes":[{
            "id":{"block":"32","tx":"0"}, "value": max,
        }]}}),
        serde_json::json!({"status":4}),
        serde_json::json!({"status":"0"}),
        serde_json::json!({"status":0,"result":{"status":5}}),
        serde_json::json!({"unknown":"provider result, not instructions"}),
        serde_json::Value::Null,
    ]
    .into_iter()
    .enumerate()
    {
        std::fs::write(mock.join("alkanes-simulate.json"), raw.to_string()).unwrap();
        let simulated = mcp.call_tool(
            10 + index as u64,
            "alkanes_simulate",
            serde_json::json!({"id":"32:0","inputs":["77", max]}),
        );
        assert_eq!(simulated["network"], "signet", "{simulated}");
        assert_eq!(simulated["advisory"], true);
        assert_eq!(simulated["inputs"][1], max);
        assert_eq!(simulated["raw"], raw);
        if index == 0 {
            assert_eq!(simulated["gas_used"], 123);
            assert_eq!(simulated["transfers"][0]["id"], "32:0");
            assert_eq!(simulated["transfers"][0]["value"], max);
        }
    }
    assert_eq!(std::fs::read(grant_path).unwrap(), grant_before);
    assert!(!dir.path().join("signet/agent-requests").exists());
    assert!(!dir.path().join("signet/transactions").exists());
    assert!(!mock.join("broadcasts.log").exists());
    assert!(!dir.path().join("testnet4").exists());
}

#[test]
fn alkanes_reads_validate_inputs_and_fail_closed_on_provider_or_sync_failure() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &[], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let config_path = dir.path().join("config.toml");
    let mock_config = std::fs::read_to_string(&config_path).unwrap();
    // A bad provider cannot mask malformed arguments or turn them into I/O.
    std::fs::write(&config_path, "network = \"signet\"\n").unwrap();
    for id in [
        // Not a name sats knows: provider names never select contracts.
        "DIESEL",
        "32:-1",
        "340282366920938463463374607431768211456:0",
    ] {
        assert_alkanes_error(
            &mcp.call_tool_raw(2, "alkanes_inspect", serde_json::json!({"id":id})),
            "invalid_alkane_id",
        );
        assert_alkanes_error(
            &mcp.call_tool_raw(
                3,
                "alkanes_simulate",
                serde_json::json!({"id":id,"inputs":[]}),
            ),
            "invalid_alkane_id",
        );
    }
    for input in [
        "",
        "-1",
        "+1",
        "1.5",
        "0x1",
        "340282366920938463463374607431768211456",
    ] {
        assert_alkanes_error(
            &mcp.call_tool_raw(
                4,
                "alkanes_simulate",
                serde_json::json!({"id":"32:0","inputs":[input]}),
            ),
            "invalid_alkane_inputs",
        );
    }
    for args in [
        serde_json::json!({"id":"32:0","inputs":[9007199254740993u64]}),
        serde_json::json!({"id":"32:0","inputs":[],"url":"https://untrusted.invalid"}),
        serde_json::json!({"id":"32:0","inputs":[],"password":"not-accepted"}),
    ] {
        let response = mcp.call_tool_raw(5, "alkanes_simulate", args);
        assert!(
            response.get("error").is_some() || response["result"]["isError"] == true,
            "{response}"
        );
    }
    assert_alkanes_reads_refused(&mut mcp, "alkanes_unavailable");
    std::fs::write(&config_path, mock_config).unwrap();
    std::fs::write(fx.mockdata.join("sync-error"), "offline").unwrap();
    assert_alkanes_error(
        &mcp.call_tool_raw(6, "alkanes_balance", serde_json::json!({})),
        "alkanes_error",
    );
    std::fs::remove_file(fx.mockdata.join("sync-error")).unwrap();
    std::fs::remove_file(fx.mockdata.join("alkanes-outpoints.json")).unwrap();
    assert_alkanes_error(
        &mcp.call_tool_raw(7, "alkanes_balance", serde_json::json!({})),
        "alkanes_error",
    );
    assert_alkanes_error(
        &mcp.call_tool_raw(8, "alkanes_inspect", serde_json::json!({"id":"32:0"})),
        "alkanes_error",
    );
    assert_alkanes_error(
        &mcp.call_tool_raw(
            9,
            "alkanes_simulate",
            serde_json::json!({"id":"32:0","inputs":[]}),
        ),
        "alkanes_error",
    );
    std::fs::write(
        &config_path,
        "network = \"signet\"\n[signet]\nchain = \"subfrost\"\n",
    )
    .unwrap();
    assert_alkanes_reads_refused(&mut mcp, "provider_error");
    assert_eq!(
        mcp.call_tool(10, "get_grant", serde_json::json!({}))["active"],
        true
    );
    assert!(!dir.path().join("signet/transactions").exists());
    assert!(!fx.mockdata.join("broadcasts.log").exists());
}

#[test]
fn alkanes_reads_check_the_provider_network_preserve_wire_precision_and_redact_failures() {
    use sats_core::bitcoin::{Network, blockdata::constants::genesis_block};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    };

    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let token = grant_token(&dir, "claude", &["--mode", "observe"]);
    let mode = Arc::new(AtomicU8::new(0));
    let mode_server = Arc::clone(&mode);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let calls_server = Arc::clone(&calls);
    let server = common::HttpServer::start_with_body(move |_request, body| {
        let rpc: serde_json::Value = serde_json::from_str(body).unwrap();
        calls_server.lock().unwrap().push(rpc.clone());
        let response = if mode_server.load(Ordering::SeqCst) == 2 {
            serde_json::json!({"jsonrpc":"2.0","id":rpc["id"],"error":{
                "code":-32000,"message":"HEADERSECRET PATHSECRET QUERYSECRET"
            }})
        } else {
            let result = match rpc["method"].as_str().unwrap() {
                "esplora_block-height" => {
                    assert_eq!(rpc["params"], serde_json::json!([0]));
                    let network = if mode_server.load(Ordering::SeqCst) == 1 {
                        Network::Testnet4
                    } else {
                        Network::Signet
                    };
                    serde_json::json!(genesis_block(network).block_hash().to_string())
                }
                "alkanes_simulate" => serde_json::json!({"status":0}),
                "metashrew_view" => serde_json::json!("0x0061736d"),
                method => panic!("read called unexpected method {method}"),
            };
            serde_json::json!({"jsonrpc":"2.0","id":rpc["id"],"result":result})
        };
        Some((200, response.to_string()))
    });
    std::fs::write(dir.path().join("config.toml"), format!(
        "network = \"signet\"\n[subfrost]\napi_key = \"HEADERSECRET\"\n[signet]\nchain = \"subfrost\"\nsubfrost_url = \"{}/PATHSECRET?key=QUERYSECRET\"\n", server.url
    )).unwrap();
    let mut mcp = McpSession::start(&dir, "claude", &token);
    handshake(&mut mcp);
    let max = u128::MAX.to_string();
    let id = format!("{max}:{max}");
    let simulated = mcp.call_tool(
        2,
        "alkanes_simulate",
        serde_json::json!({"id":id,"inputs":[max]}),
    );
    assert_eq!(simulated["status"], 0, "{simulated}");
    assert_eq!(simulated["id"], id);
    assert_eq!(simulated["inputs"][0], max);
    let requests = calls.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["method"], "esplora_block-height");
    assert_eq!(requests[1]["method"], "alkanes_simulate");
    assert_eq!(
        requests[1]["params"],
        serde_json::json!([{
            "target":{"block":max,"tx":max}, "inputs":[max],
        }])
    );
    let inspected = mcp.call_tool(3, "alkanes_inspect", serde_json::json!({"id":id}));
    assert_eq!(inspected["id"], id);
    assert_eq!(inspected["bytecode_bytes"], 4);

    for failure_mode in [1, 2] {
        mode.store(failure_mode, Ordering::SeqCst);
        let before = calls.lock().unwrap().len();
        for (index, (tool, args)) in [
            ("alkanes_balance", serde_json::json!({})),
            ("alkanes_inspect", serde_json::json!({"id":"32:0"})),
            (
                "alkanes_simulate",
                serde_json::json!({"id":"32:0","inputs":[]}),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let response = mcp.call_tool_raw(10 + index as u64, tool, args);
            assert_alkanes_error(&response, "alkanes_error");
            let rendered = response.to_string();
            for secret in ["HEADERSECRET", "PATHSECRET", "QUERYSECRET"] {
                assert!(!rendered.contains(secret), "{rendered}");
            }
            if failure_mode == 1 {
                assert!(rendered.contains("different network"), "{rendered}");
            }
        }
        let requests = calls.lock().unwrap();
        assert_eq!(requests.len(), before + 3);
        assert!(
            requests[before..]
                .iter()
                .all(|rpc| rpc["method"] == "esplora_block-height")
        );
    }
    assert!(!dir.path().join("signet/transactions").exists());
}

#[test]
fn mcp_serves_ten_tools_and_enforces_the_grant() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let token = grant_token(&dir, "claude", &["--max-tx", "10000"]);

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

    // Exactly ten tools: the agent reads, files, and observes. It
    // cannot approve, unlock, sign, or broadcast.
    mcp.send(serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let list = mcp.recv();
    let tools = list["result"]["tools"].as_array().unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "alkanes_balance",
            "alkanes_inspect",
            "alkanes_simulate",
            "check_request",
            "get_balance",
            "get_grant",
            "get_receive_address",
            "request_alkanes_send",
            "request_send",
            "request_wrap_frbtc",
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

    for name in ["alkanes_balance", "alkanes_inspect", "alkanes_simulate"] {
        assert_eq!(annotation(name, "readOnlyHint"), Some(true.into()));
        assert_eq!(annotation(name, "idempotentHint"), Some(true.into()));
        assert_eq!(annotation(name, "openWorldHint"), Some(true.into()));
    }
    let simulation = tools
        .iter()
        .find(|tool| tool["name"] == "alkanes_simulate")
        .unwrap();
    assert_eq!(
        simulation["inputSchema"]["properties"]["inputs"]["items"]["type"],
        "string"
    );
    assert_eq!(simulation["inputSchema"]["additionalProperties"], false);
    assert!(instructions.contains("request_wrap_frbtc"));

    assert_eq!(
        annotation("request_wrap_frbtc", "readOnlyHint"),
        Some(false.into())
    );
    assert_eq!(
        annotation("request_wrap_frbtc", "destructiveHint"),
        Some(false.into())
    );
    assert_eq!(
        annotation("request_wrap_frbtc", "openWorldHint"),
        Some(true.into())
    );
    let frbtc = tools
        .iter()
        .find(|tool| tool["name"] == "request_wrap_frbtc")
        .unwrap();
    assert_eq!(frbtc["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        frbtc["inputSchema"]["properties"]
            .as_object()
            .unwrap()
            .len(),
        2
    );

    assert!(instructions.contains("request_alkanes_send"));
    for hint in ["readOnlyHint", "destructiveHint", "openWorldHint"] {
        assert_eq!(annotation("request_alkanes_send", hint), Some(false.into()));
    }
    let transfer = tools
        .iter()
        .find(|tool| tool["name"] == "request_alkanes_send")
        .unwrap();
    assert_eq!(transfer["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        transfer["inputSchema"]["properties"]
            .as_object()
            .unwrap()
            .len(),
        4
    );
    for arg in ["id", "amount", "address", "idempotency_key"] {
        assert_eq!(transfer["inputSchema"]["properties"][arg]["type"], "string");
    }

    // The grant snapshot lets the agent plan.
    let grant = mcp.call_tool(3, "get_grant", serde_json::json!({}));
    assert_eq!(grant["active"], true);
    assert_eq!(grant["max_tx_sat"], 10000);
    assert_eq!(grant["mode"], "ask");
    // A grant is a credential and a scope: no budget on the wire.
    for removed in [
        "budget_sat",
        "spent_sat",
        "remaining_sat",
        "max_fee_sat",
        "tx_count",
    ] {
        assert!(grant.get(removed).is_none(), "{removed}: {grant}");
    }

    // Over the per-tx cap: a denied request, recorded, as a successful
    // tool result — terminal, with no approval path.
    let denied = mcp.request_send(4, 20_000, "big");
    assert_eq!(denied["status"], "denied", "got: {denied}");
    assert_eq!(denied["reason"], "over_max_tx");
    let big = id_of(&denied);
    let message = denied["message"].as_str().unwrap();
    assert!(message.contains("outside the grant"), "got: {message}");
    assert!(message.contains("20,000"), "got: {message}");
    assert!(message.contains("10,000"), "got: {message}");
    assert_eq!(request_record(&dir, "claude", &big)["status"], "denied");
    assert_eq!(event_kinds(&dir), ["request_received", "denied"]);

    // Each filing is its own request, denied on its own.
    let denied = mcp.request_send(5, 60_000, "huge");
    assert_eq!(denied["status"], "denied");
    assert_eq!(denied["reason"], "over_max_tx");
    assert_ne!(id_of(&denied), big);

    // A wrong-network address is a typed operational error, not a record.
    let bad = mcp.call_tool(
        6,
        "request_send",
        serde_json::json!({ "address": "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4", "amount_sat": 1, "idempotency_key": "wrong-net" }),
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
fn alkanes_send_request_is_local_keyless_human_approved_and_observed() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let mock = common::write_mock_provider(&dir);
    let funded = common::fund_wallet(&dir, &[546, 100_000]);
    std::fs::write(
        mock.join("alkanes-outpoints.json"),
        serde_json::json!({
            funded[0].to_string(): {"32:0":"5000", "2:0":"50"},
        })
        .to_string(),
    )
    .unwrap();
    let token = grant_token(&dir, "claude", &["--max-tx", "546"]);
    let mut mcp = McpSession::start(&dir, "claude", &token);
    handshake(&mut mcp);
    let seed_path = dir.path().join("seed.sealed");
    let sealed = std::fs::read(&seed_path).unwrap();
    let config_path = dir.path().join("config.toml");
    let config = std::fs::read(&config_path).unwrap();
    // Even the initial filing and idempotent retry need neither keys nor providers.
    std::fs::remove_file(&seed_path).unwrap();
    std::fs::write(&config_path, "network = \"testnet4\"\n").unwrap();
    let args = serde_json::json!({"id":"032:00","amount":"001000","address":ADDRESS,"idempotency_key":"asset"});
    let filed = mcp.call_tool(2, "request_alkanes_send", args.clone());
    assert_eq!(filed["status"], "pending_approval", "{filed}");
    assert_eq!(filed["operation"], "asset_transfer");
    assert_eq!(
        filed["asset_transfer"],
        serde_json::json!({"protocol":"alkanes","id":"32:0","amount":"1000"})
    );
    assert_eq!(filed["amount_sat"], 546);
    let id = id_of(&filed);
    assert_eq!(
        mcp.call_tool(3, "request_alkanes_send", args.clone())["request_id"],
        id
    );
    assert_eq!(request_record(&dir, "claude", &id)["network"], "signet");
    assert_eq!(request_record(&dir, "claude", &id)["format_version"], 4);
    assert_eq!(
        mcp.request_send(4, 546, "asset")["error_code"],
        "idempotency_key_conflict"
    );
    assert!(!mock.join("broadcasts.log").exists());
    assert!(!dir.path().join("signet/transactions").exists());
    std::fs::write(&config_path, config).unwrap();
    std::fs::write(&seed_path, sealed).unwrap();

    // The real terminal review must precede password input, including --json/--yes.
    #[cfg(unix)]
    {
        let output = Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/alkanes_review_pty.py"
            ))
            .arg(sats_bin())
            .arg(dir.path())
            .arg(&id)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(saved_transactions(&dir), 0);
    }
    let approval = sats_output(&dir, &["--json", "agent", "approve", &id, "--yes"]);
    assert!(
        approval.status.success(),
        "{}",
        String::from_utf8_lossy(&approval.stderr)
    );
    let review = String::from_utf8_lossy(&approval.stderr);
    for field in [
        "Asset transfer",
        "alkanes:32:0 (frBTC)",
        "1000 base units",
        "32:0: 4000",
        "2:0: 50",
        "546",
        "Fee",
        "Total",
    ] {
        assert!(review.contains(field), "missing {field}: {review}");
    }
    let result = mcp.check_request(5, &id);
    assert_eq!(result["status"], "sent", "{result}");
    assert_eq!(result["asset_transfer"], filed["asset_transfer"]);
    assert!(result["message"].as_str().unwrap().contains("confirmation"));
    assert_eq!(saved_transactions(&dir), 1);
    let broadcasts = std::fs::read(mock.join("broadcasts.log")).unwrap();
    assert_eq!(
        mcp.call_tool(6, "request_alkanes_send", args)["status"],
        "sent"
    );
    assert!(
        !sats_output(&dir, &["agent", "approve", &id, "--yes"])
            .status
            .success()
    );
    assert_eq!(
        std::fs::read(mock.join("broadcasts.log")).unwrap(),
        broadcasts
    );
}

/// A name sats knows stands in for its id, and the request records the id.
#[test]
fn alkanes_send_requests_record_a_known_name_as_its_id() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &[], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let args = |id: &str| serde_json::json!({"id":id,"amount":"1000","address":ADDRESS,"idempotency_key":"named"});
    let by_name = mcp.call_tool(2, "request_alkanes_send", args("frbtc"));
    assert_eq!(by_name["status"], "pending_approval", "{by_name}");
    assert_eq!(
        by_name["asset_transfer"],
        serde_json::json!({"protocol":"alkanes","id":"32:0","amount":"1000"})
    );
    // The same proposal by id is the same request, not a conflict.
    let by_id = mcp.call_tool(3, "request_alkanes_send", args("32:0"));
    assert_eq!(by_id["request_id"], by_name["request_id"], "{by_id}");
    assert_eq!(by_id["status"], "pending_approval");
}

#[test]
fn alkanes_send_requests_preserve_u128_reject_extra_authority_and_obey_observe() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--mode", "observe"], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let base = serde_json::json!({"id":"32:0","amount":u128::MAX.to_string(),"address":ADDRESS,"idempotency_key":"asset"});
    let denied = mcp.call_tool(2, "request_alkanes_send", base.clone());
    assert_eq!(denied["status"], "denied", "{denied}");
    assert_eq!(denied["reason"], "observe_only");
    assert_eq!(denied["asset_transfer"]["amount"], u128::MAX.to_string());
    assert_eq!(
        mcp.check_request(3, &id_of(&denied))["reason"],
        "observe_only"
    );
    assert!(
        !sats_output(&dir, &["agent", "approve", &id_of(&denied), "--yes"])
            .status
            .success()
    );
    for (field, code, value) in [
        ("id", "invalid_alkane_id", "DIESEL"),
        ("amount", "invalid_asset_amount", "0"),
        ("amount", "invalid_asset_amount", "-1"),
        (
            "amount",
            "invalid_asset_amount",
            "340282366920938463463374607431768211456",
        ),
        ("address", "invalid_address", "not-an-address"),
        ("idempotency_key", "invalid_idempotency_key", ""),
    ] {
        let mut args = base.clone();
        args[field] = value.into();
        let error = mcp.call_tool(4, "request_alkanes_send", args);
        assert_eq!(error["error_code"], code, "{error}");
    }
    let mut numeric = base.clone();
    numeric["amount"] = 9007199254740993u64.into();
    let mut forbidden: Vec<_> = [
        "amount_sat",
        "postage_sat",
        "fee_rate",
        "network",
        "protocol",
        "inputs",
        "psbt",
        "password",
        "seed",
        "allow_dust",
    ]
    .into_iter()
    .map(|field| {
        let mut args = base.clone();
        args[field] = "untrusted".into();
        args
    })
    .collect();
    forbidden.push(numeric);
    for args in forbidden {
        let response = mcp.call_tool_raw(5, "request_alkanes_send", args);
        assert!(
            response.get("error").is_some() || response["result"]["isError"] == true,
            "{response}"
        );
    }
    assert_eq!(saved_transactions(&dir), 0);
    assert!(!fx.mockdata.join("broadcasts.log").exists());
    assert!(!dir.path().join("signet/transactions").exists());
}

#[test]
fn frbtc_wrap_request_is_human_approved_and_observed_end_to_end() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let mock = common::write_mock_provider(&dir);
    common::fund_wallet(&dir, &[100_000]);
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
    let info = sats_json(&dir, &["--json", "alkanes", "frbtc", "info"]);
    assert_eq!(info["id"], "32:0", "{info}");
    let grant_output = sats_output(
        &dir,
        &["--json", "agent", "grant", "claude", "--max-tx", "10000"],
    );
    assert!(
        grant_output.status.success(),
        "{}",
        String::from_utf8_lossy(&grant_output.stderr)
    );
    let grant: serde_json::Value = serde_json::from_slice(&grant_output.stdout).unwrap();
    let token = grant["token"].as_str().unwrap();
    assert!(grant.get("alkanes_wrap").is_none());
    let mut mcp = McpSession::start(&dir, "claude", token);
    handshake(&mut mcp);
    let view = mcp.call_tool(2, "get_grant", serde_json::json!({}));
    assert!(view.get("alkanes_wrap").is_none());
    // Filing reads public contract facts without loading a seed or preparing.
    let seed_path = dir.path().join("seed.sealed");
    let sealed = std::fs::read(&seed_path).unwrap();
    std::fs::remove_file(&seed_path).unwrap();
    let config_path = dir.path().join("config.toml");
    let config = std::fs::read(&config_path).unwrap();
    let args = serde_json::json!({"amount_sat":10000,"idempotency_key":"wrap-1"});
    let filed = mcp.call_tool(3, "request_wrap_frbtc", args.clone());
    assert_eq!(filed["status"], "pending_approval", "{filed}");
    assert_eq!(filed["operation"], "frbtc_wrap");
    assert_eq!(filed["frbtc"]["signer_address"], info["signer_address"]);
    assert_eq!(filed["frbtc"]["code_hash"], info["code_hash"]);
    // Once filed, retries return that receipt even when discovery is offline.
    std::fs::write(&config_path, "network = \"signet\"\n").unwrap();
    let id = id_of(&filed);
    assert_eq!(
        mcp.call_tool(4, "request_wrap_frbtc", args.clone())["request_id"],
        id
    );
    assert_eq!(
        mcp.request_send(5, 10000, "wrap-1")["error_code"],
        "idempotency_key_conflict"
    );
    assert!(!mock.join("broadcasts.log").exists());
    assert!(!dir.path().join("signet/transactions").exists());
    std::fs::write(&config_path, config).unwrap();
    std::fs::write(&seed_path, sealed).unwrap();
    let approval = sats_output(&dir, &["--json", "agent", "approve", &id, "--yes"]);
    assert!(
        approval.status.success(),
        "{}",
        String::from_utf8_lossy(&approval.stderr)
    );
    let human_review = String::from_utf8_lossy(&approval.stderr);
    for field in ["Contract code hash", "frBTC receive", "9990", "546"] {
        assert!(
            human_review.contains(field),
            "missing {field}: {human_review}"
        );
    }
    let result = mcp.check_request(6, &id);
    assert_eq!(result["status"], "sent", "{result}");
    assert!(result["message"].as_str().unwrap().contains("confirmation"));
    assert_eq!(saved_transactions(&dir), 1);
    assert_eq!(
        mcp.call_tool(7, "get_grant", serde_json::json!({}))["active"],
        true
    );
    let broadcasts = std::fs::read(mock.join("broadcasts.log")).unwrap();
    assert_eq!(
        mcp.call_tool(8, "request_wrap_frbtc", args)["status"],
        "sent"
    );
    assert!(
        !sats_output(&dir, &["agent", "approve", &id, "--yes"])
            .status
            .success()
    );
    assert_eq!(
        std::fs::read(mock.join("broadcasts.log")).unwrap(),
        broadcasts
    );
}

#[test]
fn frbtc_requests_use_standard_grants_and_reject_arbitrary_execution_parameters() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let mock = common::write_mock_provider(&dir);
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
    let token = grant_token(&dir, "claude", &["--mode", "observe"]);
    let mut mcp = McpSession::start(&dir, "claude", &token);
    handshake(&mut mcp);
    let denied = mcp.call_tool(
        2,
        "request_wrap_frbtc",
        serde_json::json!({"amount_sat":10000,"idempotency_key":"observe"}),
    );
    assert_eq!(denied["status"], "denied", "{denied}");
    assert_eq!(denied["reason"], "observe_only", "{denied}");
    assert!(
        !sats_output(&dir, &["agent", "approve", &id_of(&denied), "--yes"])
            .status
            .success()
    );
    let invalid = mcp.call_tool(
        3,
        "request_wrap_frbtc",
        serde_json::json!({"amount_sat":1,"idempotency_key":"small"}),
    );
    assert_eq!(invalid["error_code"], "invalid_amount", "{invalid}");
    for field in ["address", "inputs", "fee_rate", "psbt", "password", "id"] {
        let mut args = serde_json::json!({"amount_sat":10000,"idempotency_key":"invalid"});
        args[field] = "untrusted".into();
        let response = mcp.call_tool_raw(4, "request_wrap_frbtc", args);
        assert!(
            response.get("error").is_some() || response["result"]["isError"] == true,
            "{response}"
        );
    }
    // Revoke the session before discovery. An unavailable provider must not
    // mask token authorization or allow a revoked session to access it.
    run_sats(&dir, &["agent", "revoke", "claude"]);
    std::fs::remove_file(mock.join("alkanes-signer.hex")).unwrap();
    assert_eq!(
        mcp.call_tool(
            5,
            "request_wrap_frbtc",
            serde_json::json!({"amount_sat":10000,"idempotency_key":"revoked"})
        )["error_code"],
        "no_grant"
    );
    assert!(!mock.join("broadcasts.log").exists());
}

#[test]
fn request_send_files_pending_and_approve_executes() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &["--max-tx", "30000"], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);

    let filed = mcp.request_send(2, 4_500, "invoice-1");
    assert_eq!(filed["status"], "pending_approval", "got: {filed}");
    let id = id_of(&filed);
    assert_eq!(filed["recipient"], ADDRESS);
    assert_eq!(filed["amount_sat"], 4_500);
    assert!(
        filed["message"]
            .as_str()
            .unwrap()
            .contains(&format!("sats agent approve {id}"))
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
        .join(format!("signet/agent-requests/claude/{id}.json"));
    let record_before = std::fs::read(&record_path).unwrap();
    let events_before = std::fs::read(dir.path().join("signet/events/log.jsonl")).unwrap();
    for call in [3, 4] {
        let view = mcp.check_request(call, &id);
        assert_eq!(view["status"], "pending_approval", "got: {view}");
        assert_eq!(view["request_id"], id);
    }
    // The idempotency key is not a request id.
    let by_key = mcp.check_request(5, "invoice-1");
    assert_eq!(by_key["status"], "not_found", "got: {by_key}");
    assert!(
        by_key["message"]
            .as_str()
            .unwrap()
            .contains("idempotency_key")
    );
    assert_eq!(std::fs::read(&record_path).unwrap(), record_before);
    assert_eq!(
        std::fs::read(dir.path().join("signet/events/log.jsonl")).unwrap(),
        events_before
    );

    // The human's queue shows it, and the human approves.
    let queue = sats_json(&dir, &["--json", "agent", "requests"]);
    assert_eq!(queue.as_array().unwrap().len(), 1);
    assert_eq!(queue[0]["id"], id);
    assert_eq!(queue[0]["status"], "pending_approval");
    let approved = approve(&dir, &id);
    assert_eq!(approved["status"], "sent", "got: {approved}");
    let txid = approved["txid"].as_str().unwrap().to_string();
    assert_eq!(approved["amount_sat"], 4_500);
    let fee = approved["fee_sat"].as_u64().unwrap();
    assert!(fee > 0);
    assert_eq!(approved["total_sat"], 4_500 + fee);
    assert!(approved.get("remaining_budget_sat").is_none(), "{approved}");

    // The agent observes the result without having done anything.
    let view = mcp.check_request(6, &id);
    assert_eq!(view["status"], "sent", "got: {view}");
    assert_eq!(view["txid"], txid);
    assert_eq!(view["fee_sat"], fee);

    // Attribution and accounting.
    let record = read_json(&dir.path().join(format!("signet/transactions/{txid}.json")));
    assert_eq!(record["status"], "broadcast");
    assert_eq!(record["origin"]["surface"], "agent");
    assert_eq!(record["origin"]["agent"], "claude");
    assert_eq!(record["origin"]["request_id"], id);
    assert_eq!(saved_transactions(&dir), 1);
    assert!(
        std::fs::read_to_string(fx.mockdata.join("broadcasts.log"))
            .unwrap()
            .contains(&txid)
    );
    assert_eq!(
        event_kinds(&dir),
        ["request_received", "approved", "signed", "broadcast"]
    );
    let log = sats_json(&dir, &["--json", "agent", "log", "--request", &id]);
    assert_eq!(log.as_array().unwrap().len(), 4);

    // Filing the same key again returns the settled request; it does
    // not file a second one and never pays twice. Passing the server id
    // back as the key is a different key: a new request, never a
    // silent replay of the settled one.
    let again = mcp.request_send(7, 4_500, "invoice-1");
    assert_eq!(again["status"], "sent");
    assert_eq!(again["txid"], txid);
    let misused = mcp.request_send(8, 4_500, &id);
    assert_eq!(misused["status"], "pending_approval", "got: {misused}");
    assert_ne!(id_of(&misused), id);
    sats_json(&dir, &["--json", "agent", "dismiss", &id_of(&misused)]);
    assert_eq!(event_kinds(&dir).len(), 6);
    assert_eq!(saved_transactions(&dir), 1, "never pays twice");

    // And a settled request cannot be approved again.
    let output = sats_output(&dir, &["agent", "approve", &id, "--yes"]);
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
    let fx = funded_setup(&dir, &[], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);

    let first = mcp.request_send(2, 1_000, "job-1");
    assert_eq!(first["status"], "pending_approval");
    let id = id_of(&first);
    let again = mcp.request_send(3, 1_000, "job-1");
    assert_eq!(again["status"], "pending_approval");
    assert_eq!(again["request_id"], id);
    assert_eq!(event_kinds(&dir), ["request_received"], "no second filing");

    let conflict = mcp.request_send(4, 2_000, "job-1");
    assert_eq!(conflict["status"], "error", "got: {conflict}");
    assert_eq!(conflict["error_code"], "idempotency_key_conflict");
    assert_eq!(conflict["request_id"], id);
    assert_eq!(
        request_record(&dir, "claude", &id)["amount_sat"],
        1_000,
        "the record keeps its intent"
    );
    assert_eq!(event_kinds(&dir), ["request_received", "conflicted"]);

    // The key is required: a call without one is rejected by the schema
    // and files nothing.
    let response = mcp.call_tool_raw(
        5,
        "request_send",
        serde_json::json!({ "address": ADDRESS, "amount_sat": 1_000 }),
    );
    assert!(
        response.get("error").is_some() || response["result"]["isError"] == true,
        "got: {response}"
    );
    let queue = sats_json(&dir, &["--json", "agent", "requests"]);
    assert_eq!(queue.as_array().unwrap().len(), 1);
    assert_eq!(event_kinds(&dir), ["request_received", "conflicted"]);

    // A malformed key is a typed error, before anything touches the disk.
    let bad = mcp.request_send(6, 1_000, "../etc");
    assert_eq!(bad["status"], "error");
    assert_eq!(bad["error_code"], "invalid_idempotency_key");
}

/// Dismissal and grant boundaries are terminal states the agent observes.
#[test]
fn dismissed_and_denied_requests_observe_as_terminal() {
    let dir = TempDir::new().unwrap();
    let other = common::foreign_address(sats_core::bitcoin::Network::Signet);
    let fx = funded_setup(&dir, &["--to", ADDRESS], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);

    let filed = mcp.request_send(2, 1_000, "later");
    assert_eq!(filed["status"], "pending_approval");
    let later = id_of(&filed);
    let dismissed = sats_json(&dir, &["--json", "agent", "dismiss", &later]);
    assert_eq!(dismissed["status"], "dismissed");
    let view = mcp.check_request(3, &later);
    assert_eq!(view["status"], "dismissed", "got: {view}");
    assert!(view["message"].as_str().unwrap().contains("dismissed"));
    assert_eq!(event_kinds(&dir), ["request_received", "dismissed"]);
    // Dismissed is terminal for the human too.
    let output = sats_output(&dir, &["agent", "approve", &later, "--yes"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already dismissed"));
    let output = sats_output(&dir, &["agent", "dismiss", &later]);
    assert!(!output.status.success());

    // A recipient off the allowlist is a hard boundary.
    let off = mcp.call_tool(
        4,
        "request_send",
        serde_json::json!({ "address": other, "amount_sat": 1_000, "idempotency_key": "stranger" }),
    );
    assert_eq!(off["status"], "denied", "got: {off}");
    assert_eq!(off["reason"], "recipient_not_allowed");
    let output = sats_output(&dir, &["agent", "approve", &id_of(&off), "--yes"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("recipient_not_allowed"));

    // Observe mode files nothing approvable.
    let observer = grant_token(&dir, "watcher", &["--mode", "observe"]);
    let mut watcher = McpSession::start(&dir, "watcher", &observer);
    handshake(&mut watcher);
    let observe = watcher.request_send(2, 1_000, "peek");
    assert_eq!(observe["status"], "denied");
    assert_eq!(observe["reason"], "observe_only");
    let balance = watcher.call_tool(3, "get_balance", serde_json::json!({}));
    assert_eq!(balance["balance_sat"], 100_000, "reading still works");

    // Another agent cannot see this agent's records through the tool.
    let cross = watcher.check_request(4, &later);
    assert_eq!(cross["status"], "not_found", "got: {cross}");
    // The same key from another agent is another request.
    let peek = watcher.request_send(5, 1_000, "later");
    assert_eq!(peek["status"], "denied", "observe-only: {peek}");
    assert_ne!(id_of(&peek), later);
    assert_eq!(mcp.check_request(6, &later)["status"], "dismissed");
    let malformed = mcp.check_request(7, "../../etc");
    assert_eq!(malformed["status"], "not_found");
    assert!(malformed["message"].as_str().unwrap().contains("malformed"));
}

#[test]
fn revocation_takes_effect_mid_session() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &[], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let filed = mcp.request_send(2, 1_000, "before");
    assert_eq!(filed["status"], "pending_approval");
    let before = id_of(&filed);
    let requests_dir = dir.path().join("signet/agent-requests/claude");
    let records = || {
        std::fs::read_dir(&requests_dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "json")
            })
            .count()
    };
    let records_before = records();

    run_sats(&dir, &["agent", "revoke", "claude"]);

    // Filing is refused without a grant; nothing is written.
    let refused = mcp.request_send(3, 1_000, "after");
    assert_eq!(refused["status"], "error", "got: {refused}");
    assert_eq!(refused["error_code"], "no_grant");
    assert_eq!(records(), records_before);
    // Every tool is cut off, reads included: the grant reports inactive,
    // and nothing else answers.
    let grant = mcp.call_tool(4, "get_grant", serde_json::json!({}));
    assert_eq!(grant["active"], false);
    assert!(grant.get("expires_at").is_none(), "{grant}");
    mcp.refused_read(5, "get_balance", "no_grant");
    mcp.refused_read(6, "get_receive_address", "no_grant");
    assert_alkanes_reads_refused(&mut mcp, "no_grant");
    let observed = mcp.check_request(7, &before);
    assert_eq!(observed["status"], "error", "{observed}");
    assert_eq!(observed["error_code"], "no_grant");
    assert!(observed.get("request_id").is_none(), "{observed}");
    // The pending request cannot execute without its grant: it is bound
    // to the revoked instance and becomes denied.
    let output = sats_output(&dir, &["agent", "approve", &before, "--yes"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("revoked"));
    assert_eq!(event_kinds(&dir), ["request_received", "denied"]);

    // A re-issued grant for the same agent never executes the old request.
    let fresh = grant_token(&dir, "claude", &[]);
    let output = sats_output(&dir, &["agent", "approve", &before, "--yes"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("denied revoked"));
    assert_eq!(saved_transactions(&dir), 0);
    let mut fresh_mcp = McpSession::start(&dir, "claude", &fresh);
    handshake(&mut fresh_mcp);
    // The old request is not the new grant's: reusing its key files a
    // new request with a new id rather than returning the old record.
    assert_eq!(fresh_mcp.check_request(2, &before)["status"], "denied");
    let refiled = fresh_mcp.request_send(3, 1_000, "before");
    assert_eq!(refiled["status"], "pending_approval", "got: {refiled}");
    assert_ne!(id_of(&refiled), before);
    assert_eq!(
        fresh_mcp.check_request(4, &id_of(&refiled))["status"],
        "pending_approval"
    );
    assert_eq!(
        fresh_mcp.request_send(5, 1_000, "after")["status"],
        "pending_approval"
    );
    assert_eq!(records(), records_before + 2);
}

/// A signature that could not be broadcast is a distinct state: the
/// reservation stands, the request never signs again, and the human's
/// rebroadcast settles it.
#[test]
fn broadcast_failure_observes_as_broadcast_pending_until_rebroadcast() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &[], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let filed = mcp.request_send(2, 1_000, "flaky");
    assert_eq!(filed["status"], "pending_approval");
    let flaky = id_of(&filed);

    std::fs::write(fx.mockdata.join("broadcast-fail"), "provider down").unwrap();
    let approved = approve(&dir, &flaky);
    assert_eq!(approved["status"], "broadcast_pending", "got: {approved}");
    let txid = approved["txid"].as_str().unwrap().to_string();
    assert!(
        approved["message"]
            .as_str()
            .unwrap()
            .contains("signed transaction is saved")
    );
    let record = read_json(&dir.path().join(format!("signet/transactions/{txid}.json")));
    assert_eq!(record["status"], "pending");
    let view = mcp.check_request(3, &flaky);
    assert_eq!(view["status"], "broadcast_pending", "got: {view}");
    assert_eq!(view["txid"], txid);
    assert_eq!(
        event_kinds(&dir),
        ["request_received", "approved", "signed", "broadcast_failed"]
    );

    // Approving again never produces a second signature.
    let output = sats_output(&dir, &["agent", "approve", &flaky, "--yes"]);
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
    assert_eq!(mcp.check_request(4, &flaky)["status"], "sent");
    assert_eq!(saved_transactions(&dir), 1, "no second signature");
    assert_eq!(request_record(&dir, "claude", &flaky)["status"], "sent");
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
    let fx = funded_setup(&dir, &[], &[100_000]);
    let stale = fx.token.clone();
    let fresh = grant_token(&dir, "claude", &[]);
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
    let _token = grant_token(&dir, "claude", &[]);

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
    let fx = funded_setup(&dir, &[], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    assert_eq!(
        mcp.request_send(2, 1_000, "ok-1")["status"],
        "pending_approval"
    );

    let _fresh = grant_token(&dir, "claude", &[]);
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
    // Nor can it read: the re-issued grant is reported inactive without
    // its limits, and the wallet tools refuse.
    let grant = mcp.call_tool(4, "get_grant", serde_json::json!({}));
    assert_eq!(grant["active"], false, "{grant}");
    assert!(grant.get("expires_at").is_none(), "{grant}");
    assert!(grant["message"].as_str().unwrap().contains("re-issued"));
    mcp.refused_read(5, "get_balance", "unauthorized");
    mcp.refused_read(6, "get_receive_address", "unauthorized");
    assert_alkanes_reads_refused(&mut mcp, "unauthorized");
    assert_eq!(
        mcp.check_request(7, "r-00000000000000000000000000000000")["error_code"],
        "unauthorized"
    );
}

/// Expiry cuts a running session off at its next call, like revocation:
/// the grant is re-read on every call, not trusted from startup.
#[test]
fn expiry_cuts_off_a_running_session() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &[], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let filed = mcp.request_send(2, 1_000, "before");
    assert_eq!(filed["status"], "pending_approval");
    let address = mcp.call_tool(3, "get_receive_address", serde_json::json!({}));
    let index = address["index"].as_u64().unwrap();

    let grant_path = dir.path().join("signet/grants/claude.json");
    let mut grant = read_json(&grant_path);
    grant["expires_at"] = serde_json::json!(1);
    std::fs::write(&grant_path, grant.to_string()).unwrap();

    let grant = mcp.call_tool(4, "get_grant", serde_json::json!({}));
    assert_eq!(grant["active"], false, "{grant}");
    assert!(grant["message"].as_str().unwrap().contains("expired"));
    mcp.refused_read(5, "get_balance", "no_grant");
    mcp.refused_read(6, "get_receive_address", "no_grant");
    assert_alkanes_reads_refused(&mut mcp, "no_grant");
    assert_eq!(
        mcp.check_request(7, &id_of(&filed))["error_code"],
        "no_grant"
    );
    // A refused read does not advance the wallet: the next live session
    // reveals the address after the one already handed out.
    let fresh = grant_token(&dir, "claude", &[]);
    let mut fresh_mcp = McpSession::start(&dir, "claude", &fresh);
    handshake(&mut fresh_mcp);
    let next = fresh_mcp.call_tool(2, "get_receive_address", serde_json::json!({}));
    assert_eq!(next["index"].as_u64().unwrap(), index + 1, "{next}");
}

/// A request filed before its grant expired is denied at approval, never
/// signed, and the agent observes the denial and its reason.
#[test]
fn a_request_pending_at_expiry_is_denied_at_approval() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &[], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let filed = mcp.request_send(2, 1_000, "late");
    assert_eq!(filed["status"], "pending_approval");
    let late = id_of(&filed);

    let grant_path = dir.path().join("signet/grants/claude.json");
    let mut grant = read_json(&grant_path);
    grant["expires_at"] = serde_json::json!(1);
    std::fs::write(&grant_path, grant.to_string()).unwrap();

    let output = sats_output(&dir, &["agent", "approve", &late, "--yes"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("expired"), "{stderr}");
    assert_eq!(request_record(&dir, "claude", &late)["status"], "denied");
    assert_eq!(saved_transactions(&dir), 0);
    assert_eq!(event_kinds(&dir), ["request_received", "denied"]);
    assert!(!dir.path().join("signet/transactions").exists());

    // The expired session observes nothing; a fresh grant's session sees
    // the old request as denied, with the reason.
    assert_eq!(mcp.check_request(3, &late)["error_code"], "no_grant");
    let fresh = grant_token(&dir, "claude", &[]);
    let mut fresh_mcp = McpSession::start(&dir, "claude", &fresh);
    handshake(&mut fresh_mcp);
    let view = fresh_mcp.check_request(2, &late);
    assert_eq!(view["status"], "denied", "{view}");
    assert_eq!(view["reason"], "expired", "{view}");
}

#[test]
fn get_balance_reports_a_failed_sync() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &[], &[100_000]);
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

#[test]
fn provider_resolution_is_deferred_until_an_mcp_chain_read() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let token = grant_token(&dir, "claude", &["--max-tx", "10000"]);
    let servers = common::write_unresolvable_providers(&dir);

    // Deferring providers must not defer authentication.
    let refused = Command::new(sats_bin())
        .args(["agent", "serve", "claude"])
        .env("SATS_DIR", dir.path())
        .env("SATS_AGENT_TOKEN", "wrong-token")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("does not match the active grant"));

    let mut mcp = McpSession::start(&dir, "claude", &token);
    handshake(&mut mcp);
    let grant = mcp.call_tool(2, "get_grant", serde_json::json!({}));
    assert_eq!(grant["active"], true);
    assert_eq!(grant["mode"], "ask");
    let first = mcp.call_tool(3, "get_receive_address", serde_json::json!({}));
    let second = mcp.call_tool(4, "get_receive_address", serde_json::json!({}));
    assert!(first["address"].as_str().unwrap().starts_with("tb1p"));
    assert_ne!(first["address"], second["address"]);
    assert_eq!(second["index"], 1);
    let filed = mcp.request_send(5, 4_500, "local-request");
    assert_eq!(filed["status"], "pending_approval");
    let id = id_of(&filed);
    let paths = [
        dir.path()
            .join(format!("signet/agent-requests/claude/{id}.json")),
        dir.path().join("signet/grants/claude.json"),
        dir.path().join("signet/events/log.jsonl"),
        dir.path().join("signet/wallet.sqlite"),
    ];
    let before: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    assert_eq!(mcp.check_request(6, &id), filed);
    assert_eq!(mcp.request_send(7, 4_500, "local-request"), filed);
    for (path, bytes) in paths.iter().zip(before) {
        assert_eq!(std::fs::read(path).unwrap(), bytes, "{}", path.display());
    }
    let conflict = mcp.request_send(8, 4_501, "local-request");
    assert_eq!(conflict["error_code"], "idempotency_key_conflict");
    // A conflicting filing keeps its existing audit event; observation and
    // an identical retry above write nothing.
    assert_eq!(event_kinds(&dir), ["request_received", "conflicted"]);
    assert_eq!(saved_transactions(&dir), 0);
    assert!(!dir.path().join("signet/transactions").exists());

    let balance = mcp.call_tool_raw(9, "get_balance", serde_json::json!({}));
    assert!(
        balance.get("error").is_some() || balance["result"]["isError"] == true,
        "{balance}"
    );
    assert!(
        balance.to_string().contains(common::UNRESOLVABLE),
        "{balance}"
    );
    assert!(
        balance["result"]["structuredContent"].is_null(),
        "no cached balance on resolution failure"
    );
    assert_eq!(
        mcp.check_request(10, &id),
        filed,
        "a chain error must not stop observation"
    );

    let denied = mcp.request_send(11, 20_000, "over-cap");
    assert_eq!(denied["status"], "denied");
    assert_eq!(denied["reason"], "over_max_tx");
    common::assert_no_provider_calls(&servers);
}

#[test]
fn provider_resolution_deferral_preserves_mcp_config_errors() {
    let dir = TempDir::new().unwrap();
    run_sats(&dir, &["init"]);
    let token = grant_token(&dir, "claude", &[]);
    for config in [
        "network = [",
        "network = 7",
        "network = \"signet\"\n[signet]\nfee_target = 0",
    ] {
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        let output = Command::new(sats_bin())
            .args(["agent", "serve", "claude"])
            .env("SATS_DIR", dir.path())
            .env("SATS_AGENT_TOKEN", &token)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("invalid config"), "{stderr}");
        assert!(!stderr.contains(common::UNRESOLVABLE), "{stderr}");
    }
}

// Provider failures on the approving side: a stalled chain provider
// refuses to plan on stale state and leaves the request pending; a
// broadcast timeout after signing is broadcast_pending with the signed
// transaction kept. Credentials never reach the human's error text.

/// Route one chain role through a real HTTP transport. `sync` makes the
/// endpoint the whole chain source; `fees` and `broadcast` keep the mock
/// sync and hand only that role to an Esplora endpoint (the mock's test
/// seam).
fn with_http_provider(dir: &TempDir, driver: &str, url: &str, role: &str) {
    if role != "sync" {
        assert_eq!(driver, "esplora");
        std::fs::write(dir.path().join("mockdata").join(format!("{role}-via")), url).unwrap();
        return;
    }
    let path = dir.path().join("config.toml");
    let mut config: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let signet = config["signet"].as_table_mut().unwrap();
    signet.insert("chain".into(), driver.into());
    if driver == "esplora" {
        signet.insert(
            "esplora".into(),
            toml::Value::try_from(serde_json::json!({ "url": url })).unwrap(),
        );
    } else {
        signet.insert("subfrost_url".into(), url.into());
    }
    std::fs::write(path, toml::to_string(&config).unwrap()).unwrap();
}

fn assert_sync_timeout(driver: &str) {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &[], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let filed = mcp.request_send(2, 1_000, "timeout");
    assert_eq!(filed["status"], "pending_approval");
    let id = id_of(&filed);
    drop(mcp);

    let http = common::HttpServer::start(|_| None);
    let url = format!("{}/PRIVATE_PATH_KEY", http.url);
    with_http_provider(&dir, driver, &url, "sync");
    let started = std::time::Instant::now();
    let output = sats_output(&dir, &["agent", "approve", &id, "--yes"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("stale state"), "got: {stderr}");
    assert!(started.elapsed() < std::time::Duration::from_secs(45));
    assert!(!stderr.contains("PRIVATE_PATH_KEY"), "got: {stderr}");
    assert_eq!(http.requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(event_kinds(&dir), ["request_received", "failed"]);
    assert_eq!(request_record(&dir, "claude", &id)["status"], "failed");
    assert_eq!(saved_transactions(&dir), 0);
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
    let fx = funded_setup(&dir, &[], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let filed = mcp.request_send(2, 1_000, "fees");
    assert_eq!(filed["status"], "pending_approval");
    let id = id_of(&filed);
    drop(mcp);

    let http = common::HttpServer::start(|request| {
        assert!(request.starts_with("GET /fee-estimates "));
        Some((503, "unavailable".into()))
    });
    with_http_provider(&dir, "esplora", &http.url, "fees");
    let output = sats_output(&dir, &["agent", "approve", &id, "--yes"]);
    assert!(!output.status.success());
    assert_eq!(http.requests.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(request_record(&dir, "claude", &id)["status"], "failed");
    assert_eq!(saved_transactions(&dir), 0);
}

#[test]
fn broadcast_timeout_keeps_the_signed_transaction() {
    let dir = TempDir::new().unwrap();
    let fx = funded_setup(&dir, &[], &[100_000]);
    let mut mcp = McpSession::start(&dir, "claude", &fx.token);
    handshake(&mut mcp);
    let filed = mcp.request_send(2, 1_000, "broadcast-timeout");
    assert_eq!(filed["status"], "pending_approval");
    let id = id_of(&filed);

    let http = common::HttpServer::start(|request| {
        assert!(request.starts_with("POST /tx "));
        None
    });
    with_http_provider(&dir, "esplora", &http.url, "broadcast");
    let approved = approve(&dir, &id);
    assert_eq!(approved["status"], "broadcast_pending", "got: {approved}");
    let txid = approved["txid"].as_str().unwrap();
    let record = read_json(&dir.path().join(format!("signet/transactions/{txid}.json")));
    assert_eq!(record["status"], "pending");
    assert_eq!(saved_transactions(&dir), 1);
    assert_eq!(
        event_kinds(&dir),
        ["request_received", "approved", "signed", "broadcast_failed"]
    );
    let view = mcp.check_request(3, &id);
    assert_eq!(view["status"], "broadcast_pending");
    assert_eq!(view["txid"], txid);
    assert_eq!(http.requests.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn preparation_receipts_redact_provider_secrets_and_observation_stays_local() {
    for driver in ["esplora", "subfrost"] {
        let dir = TempDir::new().unwrap();
        let fx = funded_setup(&dir, &[], &[100_000]);
        let mut mcp = McpSession::start(&dir, "claude", &fx.token);
        handshake(&mut mcp);
        let filed = mcp.request_send(2, 1_000, "one-filing");
        let id = id_of(&filed);
        let http = common::HttpServer::start(|_| {
            Some((401, "PATHSECRET QUERYSECRET BEARERSECRET USERSECRET".into()))
        });
        with_http_provider(
            &dir,
            driver,
            &format!("{}/PATHSECRET?unknown=QUERYSECRET", http.url),
            "sync",
        );
        let config_path = dir.path().join("config.toml");
        let mut config: toml::Value =
            toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        if driver == "esplora" {
            config["signet"]["esplora"]
                .as_table_mut()
                .unwrap()
                .insert("bearer".into(), "BEARERSECRET".into());
        } else {
            config.as_table_mut().unwrap().insert(
                "subfrost".into(),
                toml::Value::try_from(serde_json::json!({"api_key":"BEARERSECRET"})).unwrap(),
            );
        }
        std::fs::write(config_path, toml::to_string(&config).unwrap()).unwrap();
        let output = sats_output(&dir, &["agent", "approve", &id, "--yes"]);
        assert!(!output.status.success());
        let request_path = dir
            .path()
            .join(format!("signet/agent-requests/claude/{id}.json"));
        let grant_path = dir.path().join("signet/grants/claude.json");
        let events_path = dir.path().join("signet/events/log.jsonl");
        let before: Vec<_> = [&request_path, &grant_path, &events_path]
            .iter()
            .map(|p| std::fs::read(p).unwrap())
            .collect();
        for sequence in [3, 4] {
            let observed = mcp.check_request(sequence, &id);
            assert_eq!(observed["status"], "failed");
            assert_eq!(observed["request_id"], id);
            let rendered = format!(
                "{} {} {} {}",
                String::from_utf8_lossy(&output.stderr),
                observed,
                String::from_utf8_lossy(&before[0]),
                String::from_utf8_lossy(&before[2])
            );
            for secret in ["PATHSECRET", "QUERYSECRET", "BEARERSECRET", "USERSECRET"] {
                assert!(!rendered.contains(secret), "{rendered}");
            }
        }
        for (path, contents) in [&request_path, &grant_path, &events_path]
            .iter()
            .zip(before)
        {
            assert_eq!(std::fs::read(path).unwrap(), contents);
        }
        assert_eq!(http.requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(saved_transactions(&dir), 0);
        assert_eq!(event_kinds(&dir), ["request_received", "failed"]);
        std::fs::write(&request_path, "{").unwrap();
        let corrupt = mcp.check_request(5, &id);
        assert_eq!(corrupt["status"], "error");
        assert_eq!(corrupt["error_code"], "store_error");
        assert_eq!(corrupt["request_id"], id);
        assert_eq!(http.requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
