//! End-to-end CLI tests. Fully offline and deterministic: every test gets
//! its own SATS_DIR and uses SATS_PASSWORD instead of a prompt.

mod common;

use common::{init_wallet, json_stdout, sats, write_mock_provider};
use predicates::prelude::*;
use tempfile::TempDir;

#[test]
fn daemon_service_commands_have_help_and_validate_duration() {
    let dir = TempDir::new().unwrap();
    sats(&dir)
        .args(["daemon", "install", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--auto-lock"));
    sats(&dir)
        .args(["daemon", "uninstall", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("preserving wallet data"));
    sats(&dir)
        .args(["daemon", "install", "--auto-lock", "0s"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("positive duration"));
}

#[test]
fn init_receive_balance_flow() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);

    // Fresh signet taproot address, then the next index.
    sats(&dir)
        .arg("receive")
        .assert()
        .success()
        .stdout(predicate::str::contains("tb1p"))
        .stdout(predicate::str::contains("index 0"));
    let out = sats(&dir).args(["receive", "--json"]).assert().success();
    let json: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("json output");
    assert_eq!(json["index"], 1);
    assert!(json["address"].as_str().unwrap().starts_with("tb1p"));

    let out = sats(&dir)
        .args(["balance", "--offline", "--json"])
        .assert()
        .success();
    let json: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("json output");
    assert_eq!(json["balance_sat"], 0);
    assert_eq!(json["synced"], false);
}

#[test]
fn init_refuses_existing_wallet() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    sats(&dir)
        .arg("init")
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
}

/// Pull the mnemonic out of `sats init`'s stdout (indented six-word rows).
fn mnemonic_from_init(stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(stdout);
    let words: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("  "))
        .flat_map(|line| line.split_whitespace())
        .collect();
    assert_eq!(words.len(), 12, "expected a 12-word mnemonic in: {text}");
    words.join(" ")
}

#[test]
fn restore_recovers_the_same_wallet() {
    let original = TempDir::new().unwrap();
    let out = sats(&original)
        .arg("init")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let mnemonic = mnemonic_from_init(&out);
    let addr = sats(&original)
        .args(["receive", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let addr: serde_json::Value = serde_json::from_slice(&addr).unwrap();

    let restored = TempDir::new().unwrap();
    sats(&restored)
        .args(["init", "--restore"])
        .write_stdin(format!("{mnemonic}\n"))
        .assert()
        .success()
        .stdout(predicate::str::contains("phrase is valid"))
        .stdout(predicate::str::contains("wallet restored  signet"));
    let restored_addr = sats(&restored)
        .args(["receive", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let restored_addr: serde_json::Value = serde_json::from_slice(&restored_addr).unwrap();
    assert_eq!(addr["address"], restored_addr["address"]);
}

#[test]
fn restore_names_the_mistyped_word() {
    let dir = TempDir::new().unwrap();
    sats(&dir)
        .args(["init", "--restore"])
        .write_stdin("abandon abandon zzzz abandon abandon abandon abandon abandon abandon abandon abandon about\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("word 3"))
        .stderr(predicate::str::contains("Nothing was stored"));
}

#[test]
fn restore_explains_a_checksum_failure() {
    let dir = TempDir::new().unwrap();
    sats(&dir)
        .args(["init", "--restore"])
        .write_stdin(format!("{}\n", ["abandon"; 12].join(" ")))
        .assert()
        .failure()
        .stderr(predicate::str::contains("checksum"))
        .stderr(predicate::str::contains("cannot affect your funds"));
}

#[test]
fn restore_refuses_existing_seed() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    sats(&dir)
        .args(["init", "--restore"])
        .write_stdin("ignored\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn mainnet_restore_requires_typed_confirmation() {
    let dir = TempDir::new().unwrap();
    sats(&dir)
        .args(["init", "--restore", "--network", "mainnet"])
        .write_stdin("y\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("cancelled"));

    let confirmed = TempDir::new().unwrap();
    sats(&confirmed)
        .args(["init", "--restore", "--network", "mainnet"])
        .write_stdin(
            "hot wallet\nabandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about\n",
        )
        .assert()
        .success()
        .stdout(predicate::str::contains("wallet restored  mainnet"));
}

#[test]
fn init_extends_seed_to_second_network() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    sats(&dir)
        .args(["init", "--network", "testnet4"])
        .assert()
        .success()
        .stdout(predicate::str::contains("wallet extended to testnet4"));
    // Signet stays the configured default network.
    let config = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(config.contains("network = \"signet\""));
}

#[test]
fn wrong_password_is_rejected() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    sats(&dir)
        .args(["init", "--network", "mainnet"])
        .env("SATS_PASSWORD", "wrong-password")
        .assert()
        .failure()
        .stderr(predicate::str::contains("wrong password"));
}

#[test]
fn dry_run_with_no_funds_fails_cleanly() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    write_mock_provider(&dir);
    sats(&dir)
        .args([
            "send",
            "tb1pvlnw9n2zuefmxzwmuz0763uajw8nmaattkhd8002g3ekejjspxtshu2q9n",
            "25000",
            "--fee-rate",
            "2",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Insufficient funds"));
}

#[test]
fn send_rejects_wrong_network_address() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    // No provider config needed: address validation runs before any IO.
    sats(&dir)
        .args([
            "send",
            "bc1p5cyxnuxmeuwuvkwfem96lqzszd02n6xdcjrs20cac6yqjjwudpxqkedrcr",
            "1000",
            "--fee-rate",
            "2",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not valid for signet"));
}

#[test]
fn send_refuses_stale_state_when_sync_fails() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    std::fs::write(mockdata.join("sync-error"), "indexer down").unwrap();
    sats(&dir)
        .args([
            "send",
            "tb1pvlnw9n2zuefmxzwmuz0763uajw8nmaattkhd8002g3ekejjspxtshu2q9n",
            "25000",
            "--fee-rate",
            "2",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("refusing to plan on stale state"));
}

#[test]
fn guard_failure_stops_planning() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    // A configured guard that cannot answer must stop planning.
    std::fs::remove_file(mockdata.join("guard.json")).unwrap();
    sats(&dir)
        .args([
            "send",
            "tb1pvlnw9n2zuefmxzwmuz0763uajw8nmaattkhd8002g3ekejjspxtshu2q9n",
            "25000",
            "--fee-rate",
            "2",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "refusing to plan without the asset check",
        ));
}

#[test]
fn no_guards_flag_skips_the_asset_check() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    std::fs::remove_file(mockdata.join("guard.json")).unwrap();
    // With the explicit escape the pipeline proceeds past the guard and
    // fails for the ordinary reason: an empty wallet.
    sats(&dir)
        .args([
            "send",
            "tb1pvlnw9n2zuefmxzwmuz0763uajw8nmaattkhd8002g3ekejjspxtshu2q9n",
            "25000",
            "--fee-rate",
            "2",
            "--dry-run",
            "--no-guards",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Insufficient funds"));
}

#[test]
fn dry_run_conflicts_with_yes_and_export() {
    let dir = TempDir::new().unwrap();
    sats(&dir)
        .args(["send", "tb1qexample", "1000", "--dry-run", "--yes"])
        .assert()
        .code(2);
    sats(&dir)
        .args([
            "send",
            "tb1qexample",
            "1000",
            "--dry-run",
            "--export-psbt",
            "x.psbt",
        ])
        .assert()
        .code(2);
}

#[test]
fn provider_flag_overrides_config() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    // Config points at a mock that would fail; the CLI override replaces it
    // with one that works.
    let mockdata = write_mock_provider(&dir);
    std::fs::write(mockdata.join("sync-error"), "config provider down").unwrap();
    let override_data = dir.path().join("override-mockdata");
    std::fs::create_dir_all(&override_data).unwrap();
    let out = sats(&dir)
        .args([
            "balance",
            "--json",
            "--provider",
            &format!("mock=file://{}", override_data.display()),
        ])
        .assert()
        .success();
    let json: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("json output");
    assert_eq!(json["synced"], true);
}

#[test]
fn provider_flag_rejects_bad_grammar() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    sats(&dir)
        .args(["balance", "--provider", "mempool.space"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("KIND=URL"));
}

#[test]
fn two_chain_providers_are_ambiguous() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    sats(&dir)
        .args([
            "balance",
            "--provider",
            "esplora=http://a.invalid",
            "--provider",
            "subfrost=http://b.invalid",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("multiple chain.sync providers"));
}

#[test]
fn balance_tolerates_sync_failure_and_reports_it() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    std::fs::write(mockdata.join("sync-error"), "chain offline").unwrap();
    let out = sats(&dir).args(["balance", "--json"]).assert().success();
    let json: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("json output");
    assert_eq!(json["balance_sat"], 0);
    assert_eq!(json["synced"], false);
}

#[test]
fn psbt_sign_needs_an_explicit_source() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    // No hidden "newest session" default: FILE or --session is required.
    sats(&dir).args(["psbt", "sign"]).assert().code(2);
    sats(&dir)
        .args(["psbt", "sign", "--session", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no PSBT session nope"));
    sats(&dir)
        .args([
            "psbt",
            "inspect",
            dir.path().join("config.toml").to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a valid PSBT"));
}

#[test]
fn status_starts_empty() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);

    let out = sats(&dir).args(["status", "--json"]).assert().success();
    let json: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("json output");
    assert_eq!(json["pending"], serde_json::json!([]));
    assert_eq!(json["broadcast"], serde_json::json!([]));

    // --offline never touches the provider, even a broken one.
    std::fs::write(mockdata.join("sync-error"), "chain offline").unwrap();
    sats(&dir)
        .args(["status", "--offline"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no transactions"));

    sats(&dir)
        .args(["status", "deadbeef", "--offline"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no transaction deadbeef"));
}

#[test]
fn history_starts_empty() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    write_mock_provider(&dir);
    let out = sats(&dir)
        .args(["history", "--offline", "--json"])
        .assert()
        .success();
    let json: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("json output");
    assert_eq!(json, serde_json::json!([]));
}

#[test]
fn tx_broadcast_needs_an_explicit_target() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    // No hidden "newest pending" default: the target is required.
    sats(&dir).args(["tx", "broadcast"]).assert().code(2);
    sats(&dir)
        .args(["tx", "broadcast", "deadbeef"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no transaction deadbeef"));
}

#[test]
fn old_command_names_are_gone() {
    let dir = TempDir::new().unwrap();
    for args in [
        vec!["plan", "tb1qexample", "1000"],
        vec!["sign"],
        vec!["broadcast"],
        vec!["grant", "claude", "--budget", "1000"],
        vec!["grants"],
        vec!["revoke", "claude"],
        vec!["mcp", "--agent", "claude"],
    ] {
        sats(&dir).args(&args).assert().code(2);
    }
}

#[test]
fn grant_list_revoke_lifecycle() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);

    // Amount shorthand works on every sat-valued flag.
    sats(&dir)
        .args([
            "agent",
            "grant",
            "claude",
            "--budget",
            "50k",
            "--max-tx",
            "10k",
            "--max-fee",
            "1000",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("granted  claude"));

    // Grant file is a 0600 secret holding the wrapped seed.
    let grant_path = dir.path().join("signet/grants/claude.json");
    assert!(grant_path.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&grant_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let out = sats(&dir)
        .args(["agent", "list", "--json"])
        .assert()
        .success();
    let json: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(json[0]["agent"], "claude");
    assert_eq!(json[0]["budget_sat"], 50000);
    assert_eq!(json[0]["remaining_sat"], 50000);
    assert_eq!(json[0]["max_tx_sat"], 10000);
    // Key material must never appear on a read surface.
    assert!(json[0].get("grant_key").is_none());
    assert!(json[0].get("wrapped_seed").is_none());

    sats(&dir)
        .args(["agent", "revoke", "claude"])
        .assert()
        .success()
        .stdout(predicate::str::contains("revoked  claude"));
    assert!(
        !grant_path.exists(),
        "revocation must delete the grant file"
    );

    sats(&dir)
        .args(["agent", "revoke", "claude"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no grant"));
}

/// A grant with only a budget still gets a fee cap: without one, a bad
/// fee estimate can burn the whole budget as miner fees. The default is
/// max(2% of budget, 1000 sat), clamped to the budget; lifting it takes
/// the explicit `--no-max-fee`.
#[test]
fn grant_defaults_a_fee_cap_and_no_max_fee_opts_out() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);

    let granted_fee = |args: &[&str]| -> serde_json::Value {
        let mut cmd_args = vec!["--json", "agent", "grant", "claude"];
        cmd_args.extend_from_slice(args);
        let out = sats(&dir).args(&cmd_args).assert().success();
        let grant: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
        grant["max_fee_sat"].clone()
    };

    assert_eq!(granted_fee(&["--budget", "50k"]), 1_000, "floor");
    assert_eq!(granted_fee(&["--budget", "200k"]), 4_000, "2% of budget");
    assert_eq!(granted_fee(&["--budget", "500"]), 500, "clamped to budget");
    assert_eq!(granted_fee(&["--budget", "50k", "--max-fee", "250"]), 250);
    assert_eq!(
        granted_fee(&["--budget", "50k", "--no-max-fee"]),
        serde_json::Value::Null,
        "lifting the cap is explicit"
    );

    // The two fee flags contradict each other; clap refuses the pair.
    sats(&dir)
        .args([
            "agent",
            "grant",
            "claude",
            "--budget",
            "50k",
            "--max-fee",
            "250",
            "--no-max-fee",
        ])
        .assert()
        .code(2);

    // The human output names the default and both escape hatches.
    sats(&dir)
        .args(["agent", "grant", "claude", "--budget", "50k"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Max fee"))
        .stdout(predicate::str::contains("default"))
        .stdout(predicate::str::contains("--no-max-fee"));
}

#[test]
fn grant_requires_correct_password() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    sats(&dir)
        .args(["agent", "grant", "claude", "--budget", "1000"])
        .env("SATS_PASSWORD", "not-the-password")
        .assert()
        .failure()
        .stderr(predicate::str::contains("wrong password"));
}

#[test]
fn grant_rejects_bad_inputs() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    sats(&dir)
        .args(["agent", "grant", "Bad Name!", "--budget", "1000"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("agent name"));
    sats(&dir)
        .args(["agent", "grant", "claude", "--budget", "0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("budget"));
    sats(&dir)
        .args([
            "agent", "grant", "claude", "--budget", "1000", "--for", "soon",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid --for"));
    // Shorthand must land on whole sats.
    sats(&dir)
        .args(["agent", "grant", "claude", "--budget", "1.2345k"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("whole number"));
    // --expires still works as an alias for --for.
    sats(&dir)
        .args([
            "agent",
            "grant",
            "claude",
            "--budget",
            "1000",
            "--expires",
            "2h",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("granted  claude"));
}

#[test]
fn missing_wallet_points_to_init() {
    let dir = TempDir::new().unwrap();
    sats(&dir)
        .arg("receive")
        .assert()
        .failure()
        .stderr(predicate::str::contains("run: sats init"));
}

#[test]
fn agent_requests_lists_pending_denials() {
    let dir = TempDir::new().unwrap();
    let requests_dir = dir.path().join("signet/agent-requests/claude");
    std::fs::create_dir_all(&requests_dir).unwrap();
    // Fabricate records the way the MCP server writes them: one denied
    // (pending review), one already sent.
    let denied = serde_json::json!({
        "format_version": 1,
        "id": "k-big-1",
        "network": "signet",
        "agent": "claude",
        "client_request_id": "big-1",
        "recipient": common::ADDRESS,
        "amount_sat": 20_000,
        "intent_digest": "d".repeat(64),
        "created_at": 1_000,
        "updated_at": 1_001,
        "outcome": {
            "status": "denied",
            "deny": { "reason": "over_max_tx", "requested_sat": 20_000, "max_tx_sat": 10_000 },
            "resolved_at": 1_001,
        },
    });
    let sent = serde_json::json!({
        "format_version": 1,
        "id": "r-aa00bb11",
        "network": "signet",
        "agent": "claude",
        "recipient": common::ADDRESS,
        "amount_sat": 4_500,
        "intent_digest": "e".repeat(64),
        "created_at": 900,
        "updated_at": 950,
        "outcome": { "status": "sent", "txid": "ab".repeat(32), "fee_sat": 281, "resolved_at": 950 },
    });
    std::fs::write(requests_dir.join("k-big-1.json"), denied.to_string()).unwrap();
    std::fs::write(requests_dir.join("r-aa00bb11.json"), sent.to_string()).unwrap();
    std::fs::write(requests_dir.join("garbage.json"), b"not json").unwrap();

    // Default view: pending denials only; unreadable files warn, not fail.
    let pending = json_stdout(
        sats(&dir)
            .args(["agent", "requests", "--json"])
            .assert()
            .success()
            .stderr(predicate::str::contains("skipping unreadable")),
    );
    let pending = pending.as_array().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["id"], "k-big-1");
    assert_eq!(pending[0]["outcome"]["deny"]["reason"], "over_max_tx");

    // --all includes the resolved one.
    let all = json_stdout(
        sats(&dir)
            .args(["agent", "requests", "--all", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(all.as_array().unwrap().len(), 2);

    // The human table names the pending request.
    sats(&dir)
        .args(["agent", "requests"])
        .assert()
        .success()
        .stdout(predicate::str::contains("k-big-1"))
        .stdout(predicate::str::contains("denied over_max_tx"));
}

#[test]
fn agent_log_renders_events_and_filters_by_request() {
    let dir = TempDir::new().unwrap();
    let events_dir = dir.path().join("signet/events");
    std::fs::create_dir_all(&events_dir).unwrap();
    let line = |request_id: &str, event: serde_json::Value| {
        let mut object = serde_json::json!({
            "format_version": 1,
            "at": 1_000,
            "network": "signet",
            "agent": "claude",
            "request_id": request_id,
            "intent_digest": "d".repeat(64),
        });
        object
            .as_object_mut()
            .unwrap()
            .extend(event.as_object().unwrap().clone());
        object.to_string()
    };
    let log = [
        line(
            "k-big-1",
            serde_json::json!({ "event": "request_received", "recipient": common::ADDRESS, "amount_sat": 20_000 }),
        ),
        line(
            "k-big-1",
            serde_json::json!({ "event": "denied", "stage": "precheck",
                "deny": { "reason": "over_max_tx", "requested_sat": 20_000, "max_tx_sat": 10_000 } }),
        ),
        line("r-aa00bb11", serde_json::json!({ "event": "replayed" })),
        // A kind from a future sats: shown raw, never hidden or fatal.
        line(
            "k-future-1",
            serde_json::json!({ "event": "quantum_settled", "detail": 42 }),
        ),
        // A torn tail line, as a crash mid-append would leave.
        "{\"format_version\":1,\"at\":1".to_string(),
    ]
    .join("\n");
    std::fs::write(events_dir.join("log.jsonl"), log).unwrap();
    // The --request filter resolves ids through the request records.
    let requests_dir = dir.path().join("signet/agent-requests/claude");
    std::fs::create_dir_all(&requests_dir).unwrap();
    std::fs::write(
        requests_dir.join("k-big-1.json"),
        serde_json::json!({
            "format_version": 1, "id": "k-big-1", "network": "signet",
            "agent": "claude", "recipient": common::ADDRESS, "amount_sat": 20_000,
            "intent_digest": "d".repeat(64), "created_at": 1_000, "updated_at": 1_000,
        })
        .to_string(),
    )
    .unwrap();

    let events = json_stdout(
        sats(&dir)
            .args(["agent", "log", "--json"])
            .assert()
            .success()
            .stderr(predicate::str::contains("event log line that is not JSON")),
    );
    let events = events.as_array().unwrap();
    assert_eq!(events.len(), 4, "the unknown kind is reported, raw");
    assert_eq!(events[3]["event"], "quantum_settled");
    assert_eq!(events[3]["detail"], 42);

    let filtered = json_stdout(
        sats(&dir)
            .args(["agent", "log", "--request", "k-big", "--json"])
            .assert()
            .success(),
    );
    let filtered = filtered.as_array().unwrap();
    assert_eq!(filtered.len(), 2);
    assert!(filtered.iter().all(|e| e["request_id"] == "k-big-1"));

    // Human render: one line per event with the typed denial code, the
    // future kind shown raw with a warning, and no abort.
    sats(&dir)
        .args(["agent", "log"])
        .assert()
        .success()
        .stdout(predicate::str::contains("over_max_tx at precheck"))
        .stdout(predicate::str::contains("quantum_settled"))
        .stdout(predicate::str::contains("unknown to this sats"))
        .stdout(predicate::str::contains("written by a newer sats"));

    // --limit keeps the newest events — unknown lines count like any
    // other, because hiding them would misstate what happened last.
    let limited = json_stdout(
        sats(&dir)
            .args(["agent", "log", "--limit", "1", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(limited.as_array().unwrap().len(), 1);
    assert_eq!(limited.as_array().unwrap()[0]["event"], "quantum_settled");
}

/// The hard ceiling flag validates its band geometry and lands in both
/// the JSON emission and the grant file.
#[test]
fn grant_hard_ceiling_validates_and_persists() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);

    // A ceiling below the automatic cap is a negative-width ask band.
    sats(&dir)
        .args([
            "agent",
            "grant",
            "claude",
            "--budget",
            "50000",
            "--max-tx",
            "10000",
            "--ask-max-tx",
            "5000",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must be at least"));
    assert!(
        !dir.path().join("signet/grants/claude.json").exists(),
        "a refused grant writes nothing"
    );

    let issued = json_stdout(
        sats(&dir)
            .args([
                "--json",
                "agent",
                "grant",
                "claude",
                "--budget",
                "50000",
                "--max-tx",
                "10000",
                "--ask-max-tx",
                "25000",
            ])
            .assert()
            .success(),
    );
    assert_eq!(issued["ask_max_tx_sat"], 25_000);
    let grant: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("signet/grants/claude.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(grant["ask_max_tx_sat"], 25_000);
    assert_eq!(grant["format_version"], 3);
}

/// `--watch` is the trusted discovery channel: it must announce each
/// newly pending approvable request exactly once, stay quiet for
/// everything else, and keep streaming past torn records.
#[test]
fn agent_requests_watch_streams_newly_pending_asks() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = TempDir::new().unwrap();
    init_wallet(&dir); // approving later needs the sealed seed

    let fabricate_hard = |id: &str| {
        let requests_dir = dir.path().join("signet/agent-requests/claude");
        std::fs::create_dir_all(&requests_dir).unwrap();
        let record = serde_json::json!({
            "format_version": 1, "id": id, "network": "signet", "agent": "claude",
            "recipient": common::ADDRESS, "amount_sat": 30_000,
            "intent_digest": "d".repeat(64), "created_at": 1_000, "updated_at": 1_000,
            "outcome": {
                "status": "denied",
                "deny": { "reason": "over_ask_max", "requested_sat": 30_000, "ask_max_tx_sat": 25_000 },
                "resolved_at": 1_001,
            },
        });
        std::fs::write(requests_dir.join(format!("{id}.json")), record.to_string()).unwrap();
    };

    // One request is already waiting before the watcher starts.
    fabricate_denied_request(&dir, "k-w-0");

    let mut child = Command::new(env!("CARGO_BIN_EXE_sats"))
        .args(["--json", "agent", "requests", "--watch"])
        .env("SATS_DIR", dir.path())
        .env("SATS_PASSWORD", common::PASSWORD)
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let expect_id = |lines: &mpsc::Receiver<String>, id: &str| {
        let line = lines
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("expected a line for {id}"));
        let value: serde_json::Value = serde_json::from_str(&line).expect("JSONL stream");
        assert_eq!(value["id"], id, "line: {line}");
        value
    };
    let expect_quiet = |lines: &mpsc::Receiver<String>| {
        if let Ok(line) = lines.recv_timeout(Duration::from_millis(2_500)) {
            panic!("expected silence, got: {line}");
        }
    };

    // Startup renders what is already pending, once.
    expect_id(&lines, "k-w-0");

    // A new ask announces once; repeats and rewrites stay silent.
    fabricate_denied_request(&dir, "k-w-1");
    expect_id(&lines, "k-w-1");
    fabricate_denied_request(&dir, "k-w-1"); // rewritten, still pending
    expect_quiet(&lines);

    // Approving removes it from the queue silently; only the next new
    // ask prints.
    sats(&dir)
        .args(["agent", "approve", "k-w-1", "--max-fee", "100"])
        .assert()
        .success();
    fabricate_denied_request(&dir, "k-w-2");
    expect_id(&lines, "k-w-2");

    // A hard denial is not "awaiting approval" and never announces.
    fabricate_hard("k-w-3");
    expect_quiet(&lines);

    // A torn record is skipped, and the stream survives it.
    std::fs::write(
        dir.path().join("signet/agent-requests/claude/k-torn.json"),
        "{ not json",
    )
    .unwrap();
    fabricate_denied_request(&dir, "k-w-4");
    expect_id(&lines, "k-w-4");

    child.kill().unwrap();
    let _ = child.wait();

    // The human rendering names the exact approval command.
    let mut human = Command::new(env!("CARGO_BIN_EXE_sats"))
        .args(["agent", "requests", "--watch"])
        .env("SATS_DIR", dir.path())
        .env("SATS_PASSWORD", common::PASSWORD)
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(human.stdout.take().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(
        line.contains("sats agent approve k-w-"),
        "watch line must name the command: {line}"
    );
    human.kill().unwrap();
    let _ = human.wait();

    // And --all remains the home of hard denials.
    let all = json_stdout(
        sats(&dir)
            .args(["agent", "requests", "--all", "--json"])
            .assert()
            .success(),
    );
    assert!(
        all.as_array()
            .unwrap()
            .iter()
            .any(|request| request["id"] == "k-w-3"),
        "hard denials stay reviewable with --all"
    );
}

/// Mode transitions carry the attenuation rule: tightening never asks
/// for the password, widening always does, and both land in the file
/// and the causal log.
#[test]
fn agent_mode_transitions_gate_on_widening() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let issued = json_stdout(
        sats(&dir)
            .args([
                "--json", "agent", "grant", "claude", "--budget", "50000", "--mode", "ask",
            ])
            .assert()
            .success(),
    );
    assert_eq!(issued["mode"], "ask");
    let grant_mode = |dir: &TempDir| {
        serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(dir.path().join("signet/grants/claude.json")).unwrap(),
        )
        .unwrap()["mode"]
            .clone()
    };
    assert_eq!(grant_mode(&dir), "ask");

    // Tightening works even with the wrong password on hand: no prompt
    // runs at all.
    sats(&dir)
        .args(["agent", "mode", "claude", "observe"])
        .env("SATS_PASSWORD", "wrong-password")
        .assert()
        .success()
        .stdout(predicate::str::contains("observe mode"));
    assert_eq!(grant_mode(&dir), "observe");

    // Widening with the wrong password fails and changes nothing.
    sats(&dir)
        .args(["agent", "mode", "claude", "auto"])
        .env("SATS_PASSWORD", "wrong-password")
        .assert()
        .failure()
        .stderr(predicate::str::contains("wrong password"));
    assert_eq!(grant_mode(&dir), "observe");

    // Widening with the right password succeeds; repeating is a no-op.
    sats(&dir)
        .args(["agent", "mode", "claude", "auto"])
        .assert()
        .success();
    assert_eq!(grant_mode(&dir), "auto");
    sats(&dir)
        .args(["agent", "mode", "claude", "auto"])
        .assert()
        .success()
        .stdout(predicate::str::contains("already"));
    sats(&dir)
        .args(["agent", "mode", "claude", "sideways"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown mode"));

    // Both real transitions are attributable in the log, with direction.
    let log = json_stdout(
        sats(&dir)
            .args(["agent", "log", "--json"])
            .assert()
            .success(),
    );
    let changes: Vec<&serde_json::Value> = log
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event"] == "mode_changed")
        .collect();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0]["from"], "ask");
    assert_eq!(changes[0]["to"], "observe");
    assert_eq!(changes[0]["widened"], false);
    assert_eq!(changes[1]["from"], "observe");
    assert_eq!(changes[1]["to"], "auto");
    assert_eq!(changes[1]["widened"], true);

    // The list shows the mode column.
    let list = json_stdout(
        sats(&dir)
            .args(["--json", "agent", "list"])
            .assert()
            .success(),
    );
    assert_eq!(list.as_array().unwrap()[0]["mode"], "auto");
}

/// Allowlist edits carry the attenuation rule (allow = password,
/// disallow = none), normalize at the boundary, and never let history
/// or approvals change the list.
#[test]
fn agent_allowlist_edits_gate_on_widening() {
    use bdk_wallet::bitcoin::Network;

    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let stranger = common::foreign_address(Network::Signet);

    // Wrong-network entries are refused at creation, before anything is
    // written; textual variants normalize and deduplicate.
    sats(&dir)
        .args([
            "agent",
            "grant",
            "claude",
            "--budget",
            "50000",
            "--to",
            "bc1p5cyxnuxmeuwuvkwfem96lqzszd02n6xdcjrs20cac6yqjjwudpxqkedrcr",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not valid for signet"));
    let upper = common::ADDRESS.to_uppercase();
    let issued = json_stdout(
        sats(&dir)
            .args([
                "--json",
                "agent",
                "grant",
                "claude",
                "--budget",
                "50000",
                "--to",
                &upper,
                "--to",
                common::ADDRESS,
            ])
            .assert()
            .success(),
    );
    let listed = issued["allowed_recipients"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "variants normalize to one entry");
    assert_eq!(listed[0], common::ADDRESS);

    let allowed_in_file = |dir: &TempDir| {
        serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(dir.path().join("signet/grants/claude.json")).unwrap(),
        )
        .unwrap()["allowed_recipients"]
            .as_array()
            .map(|l| l.len())
    };

    // Widening with the wrong password fails and writes nothing.
    sats(&dir)
        .args(["agent", "allow", "claude", &stranger])
        .env("SATS_PASSWORD", "wrong-password")
        .assert()
        .failure()
        .stderr(predicate::str::contains("wrong password"));
    assert_eq!(allowed_in_file(&dir), Some(1));

    // With the right password it lands; re-allowing is a passwordless
    // no-op.
    sats(&dir)
        .args(["agent", "allow", "claude", &stranger])
        .assert()
        .success();
    assert_eq!(allowed_in_file(&dir), Some(2));
    sats(&dir)
        .args(["agent", "allow", "claude", &stranger])
        .env("SATS_PASSWORD", "wrong-password")
        .assert()
        .success()
        .stdout(predicate::str::contains("already"));

    // Tightening never prompts — wrong password on hand, still fine —
    // and emptying the list means every recipient asks.
    sats(&dir)
        .args(["agent", "disallow", "claude", &stranger])
        .env("SATS_PASSWORD", "wrong-password")
        .assert()
        .success();
    assert_eq!(allowed_in_file(&dir), Some(1));
    sats(&dir)
        .args(["agent", "disallow", "claude", common::ADDRESS])
        .env("SATS_PASSWORD", "wrong-password")
        .assert()
        .success()
        .stdout(predicate::str::contains("every recipient asks"));
    assert_eq!(allowed_in_file(&dir), Some(0));

    // A grant with no allowlist: allow is an informative no-op, disallow
    // refuses — an allowlist cannot express "all except".
    sats(&dir)
        .args(["agent", "grant", "ghost", "--budget", "1000"])
        .assert()
        .success();
    sats(&dir)
        .args(["agent", "allow", "ghost", common::ADDRESS])
        .env("SATS_PASSWORD", "wrong-password")
        .assert()
        .success()
        .stdout(predicate::str::contains("already allowed"));
    sats(&dir)
        .args(["agent", "disallow", "ghost", common::ADDRESS])
        .assert()
        .failure()
        .stderr(predicate::str::contains("all except"));

    // Both edits are attributable control-plane events.
    let log = json_stdout(
        sats(&dir)
            .args(["agent", "log", "--json"])
            .assert()
            .success(),
    );
    let kinds: Vec<&str> = log
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["event"].as_str())
        .filter(|k| k.starts_with("recipient_"))
        .collect();
    assert_eq!(
        kinds,
        [
            "recipient_allowed",
            "recipient_disallowed",
            "recipient_disallowed"
        ]
    );
}

fn fabricate_denied_request(dir: &TempDir, id: &str) {
    let requests_dir = dir.path().join("signet/agent-requests/claude");
    std::fs::create_dir_all(&requests_dir).unwrap();
    let request = serde_json::json!({
        "format_version": 1,
        "id": id,
        "network": "signet",
        "agent": "claude",
        "recipient": common::ADDRESS,
        "amount_sat": 20_000,
        "intent_digest": "d".repeat(64),
        "created_at": 1_000,
        "updated_at": 1_001,
        "outcome": {
            "status": "denied",
            "deny": { "reason": "over_max_tx", "requested_sat": 20_000, "max_tx_sat": 10_000 },
            "resolved_at": 1_001,
        },
    });
    std::fs::write(requests_dir.join(format!("{id}.json")), request.to_string()).unwrap();
}

#[test]
fn approve_requires_the_password_and_arms_a_single_use_exception() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    fabricate_denied_request(&dir, "k-big-1");

    // The wrong password is a hard failure that writes no approval.
    sats(&dir)
        .env("SATS_PASSWORD", "wrong")
        .args(["agent", "approve", "k-big-1", "--max-fee", "500"])
        .assert()
        .failure();
    let request: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("signet/agent-requests/claude/k-big-1.json")).unwrap(),
    )
    .unwrap();
    assert!(request.get("approval").is_none());

    // This denial recorded no fee estimate, so the ceiling is explicit.
    sats(&dir)
        .args(["agent", "approve", "k-big-1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--max-fee"));

    let approved = json_stdout(
        sats(&dir)
            .args(["agent", "approve", "k-big-1", "--max-fee", "500", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(approved["id"], "k-big-1");
    assert_eq!(approved["max_fee_sat"], 500);
    assert_eq!(approved["replaced"], false);
    let request: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("signet/agent-requests/claude/k-big-1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(request["approval"]["max_fee_sat"], 500);
    assert_eq!(request["approval"]["intent_digest"], "d".repeat(64));

    // An ambiguous prefix refuses rather than guessing.
    fabricate_denied_request(&dir, "k-big-2");
    sats(&dir)
        .args(["agent", "approve", "k-big", "--max-fee", "500"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("ambiguous"));
}

#[test]
fn deny_dismisses_and_revokes_the_unconsumed_approval() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    fabricate_denied_request(&dir, "k-big-1");
    sats(&dir)
        .args(["agent", "approve", "k-big-1", "--max-fee", "500"])
        .assert()
        .success();

    let denied = json_stdout(
        sats(&dir)
            .args(["agent", "deny", "k-big-1", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(denied["dismissed"], true);
    assert_eq!(denied["approval_revoked"], true);
    let request: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("signet/agent-requests/claude/k-big-1.json")).unwrap(),
    )
    .unwrap();
    assert!(request.get("approval").is_none());
    assert!(request["dismissed_at"].is_u64());

    // Dismissed requests leave the default review queue.
    let pending = json_stdout(
        sats(&dir)
            .args(["agent", "requests", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(pending.as_array().unwrap().len(), 0);
}
