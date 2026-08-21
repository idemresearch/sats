//! End-to-end CLI tests. Fully offline and deterministic: every test gets
//! its own SATS_DIR and uses SATS_PASSWORD instead of a prompt.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

const PASSWORD: &str = "integration-test-pw";

fn sats(dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("sats").unwrap();
    cmd.env("SATS_DIR", dir.path())
        .env("SATS_PASSWORD", PASSWORD)
        .env("NO_COLOR", "1");
    cmd
}

fn init_wallet(dir: &TempDir) {
    sats(dir)
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("wallet created"));
}

/// Point the config at the hermetic mock chain provider (no network).
/// Returns the mock data directory controlling its behavior.
fn write_mock_provider(dir: &TempDir) -> std::path::PathBuf {
    let mockdata = dir.path().join("mockdata");
    std::fs::create_dir_all(&mockdata).unwrap();
    // The mock driver is also a guard, and guards fail closed on a missing
    // answer — give it an empty one by default.
    std::fs::write(mockdata.join("guard.json"), r#"{"protected": []}"#).unwrap();
    let config = format!(
        "network = \"signet\"\n\n[providers.mock]\ndriver = \"mock\"\nnetwork = \"signet\"\nurl = \"file://{}\"\n",
        mockdata.display()
    );
    std::fs::write(dir.path().join("config.toml"), config).unwrap();
    mockdata
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
fn plan_with_no_funds_fails_cleanly() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    write_mock_provider(&dir);
    sats(&dir)
        .args([
            "plan",
            "tb1pvlnw9n2zuefmxzwmuz0763uajw8nmaattkhd8002g3ekejjspxtshu2q9n",
            "25000",
            "--fee-rate",
            "2",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Insufficient funds"));
}

#[test]
fn plan_rejects_wrong_network_address() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    // No provider config needed: address validation runs before any IO.
    sats(&dir)
        .args([
            "plan",
            "bc1p5cyxnuxmeuwuvkwfem96lqzszd02n6xdcjrs20cac6yqjjwudpxqkedrcr",
            "1000",
            "--fee-rate",
            "2",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not valid for signet"));
}

#[test]
fn plan_refuses_stale_state_when_sync_fails() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    std::fs::write(mockdata.join("sync-error"), "indexer down").unwrap();
    sats(&dir)
        .args([
            "plan",
            "tb1pvlnw9n2zuefmxzwmuz0763uajw8nmaattkhd8002g3ekejjspxtshu2q9n",
            "25000",
            "--fee-rate",
            "2",
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
            "plan",
            "tb1pvlnw9n2zuefmxzwmuz0763uajw8nmaattkhd8002g3ekejjspxtshu2q9n",
            "25000",
            "--fee-rate",
            "2",
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
            "plan",
            "tb1pvlnw9n2zuefmxzwmuz0763uajw8nmaattkhd8002g3ekejjspxtshu2q9n",
            "25000",
            "--fee-rate",
            "2",
            "--no-guards",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Insufficient funds"));
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
fn sign_and_broadcast_without_sessions_point_to_next_step() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    sats(&dir)
        .arg("sign")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "no unsigned PSBT sessions — run: sats plan",
        ));
    sats(&dir)
        .arg("broadcast")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no pending transactions"));

    sats(&dir)
        .args(["broadcast", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--transaction"));
}

#[test]
fn grant_list_revoke_lifecycle() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);

    // Amount shorthand works on every sat-valued flag.
    sats(&dir)
        .args([
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

    let out = sats(&dir).args(["grants", "--json"]).assert().success();
    let json: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(json[0]["agent"], "claude");
    assert_eq!(json[0]["budget_sat"], 50000);
    assert_eq!(json[0]["remaining_sat"], 50000);
    assert_eq!(json[0]["max_tx_sat"], 10000);
    // Key material must never appear on a read surface.
    assert!(json[0].get("grant_key").is_none());
    assert!(json[0].get("wrapped_seed").is_none());

    sats(&dir)
        .args(["revoke", "claude"])
        .assert()
        .success()
        .stdout(predicate::str::contains("revoked  claude"));
    assert!(
        !grant_path.exists(),
        "revocation must delete the grant file"
    );

    sats(&dir)
        .args(["revoke", "claude"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no grant"));
}

#[test]
fn grant_requires_correct_password() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    sats(&dir)
        .args(["grant", "claude", "--budget", "1000"])
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
        .args(["grant", "Bad Name!", "--budget", "1000"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("agent name"));
    sats(&dir)
        .args(["grant", "claude", "--budget", "0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("budget"));
    sats(&dir)
        .args(["grant", "claude", "--budget", "1000", "--for", "soon"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid --for"));
    // Shorthand must land on whole sats.
    sats(&dir)
        .args(["grant", "claude", "--budget", "1.2345k"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("whole number"));
    // --expires still works as an alias for --for.
    sats(&dir)
        .args(["grant", "claude", "--budget", "1000", "--expires", "2h"])
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
