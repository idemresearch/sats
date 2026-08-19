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
    sats(dir).arg("init").assert().success().stdout(predicate::str::contains("wallet created"));
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

    let out = sats(&dir).args(["balance", "--offline", "--json"]).assert().success();
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
fn missing_wallet_points_to_init() {
    let dir = TempDir::new().unwrap();
    sats(&dir)
        .arg("receive")
        .assert()
        .failure()
        .stderr(predicate::str::contains("run: sats init"));
}
