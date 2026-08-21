//! Shared helpers for the CLI integration tests. Fully offline and
//! deterministic: every test gets its own SATS_DIR, uses SATS_PASSWORD
//! instead of a prompt, and talks to the file-driven mock provider.
#![allow(dead_code)]

use assert_cmd::Command;
use bdk_wallet::bitcoin::hashes::Hash;
use bdk_wallet::bitcoin::{Amount, BlockHash, Network, OutPoint};
use bdk_wallet::chain::{BlockId, ConfirmationBlockTime};
use bdk_wallet::test_utils::{insert_checkpoint, receive_output};
use predicates::prelude::*;
use tempfile::TempDir;

pub const PASSWORD: &str = "integration-test-pw";

/// A valid signet taproot address for send targets.
pub const ADDRESS: &str = "tb1pvlnw9n2zuefmxzwmuz0763uajw8nmaattkhd8002g3ekejjspxtshu2q9n";

pub fn sats(dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("sats").unwrap();
    cmd.env("SATS_DIR", dir.path())
        .env("SATS_PASSWORD", PASSWORD)
        .env("NO_COLOR", "1");
    cmd
}

pub fn init_wallet(dir: &TempDir) {
    sats(dir)
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("wallet created"));
}

/// Point the config at the hermetic mock chain provider (no network).
/// Returns the mock data directory controlling its behavior.
pub fn write_mock_provider(dir: &TempDir) -> std::path::PathBuf {
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

/// Seed the CLI's persisted signet wallet with confirmed UTXOs. The mock
/// provider's sync is a no-op, so these funds survive CLI syncs.
pub fn fund_wallet(dir: &TempDir, values_sat: &[u64]) -> Vec<OutPoint> {
    let db = dir.path().join("signet/wallet.sqlite");
    let mut conn = rusqlite::Connection::open(&db).unwrap();
    let mut wallet = bdk_wallet::Wallet::load()
        .check_network(Network::Signet)
        .load_wallet(&mut conn)
        .unwrap()
        .expect("run init_wallet first");
    let block_900 = BlockId {
        height: 900,
        hash: BlockHash::all_zeros(),
    };
    insert_checkpoint(&mut wallet, block_900);
    insert_checkpoint(
        &mut wallet,
        BlockId {
            height: 1_000,
            hash: BlockHash::all_zeros(),
        },
    );
    let outpoints = values_sat
        .iter()
        .map(|v| {
            receive_output(
                &mut wallet,
                Amount::from_sat(*v),
                ConfirmationBlockTime {
                    block_id: block_900,
                    confirmation_time: 100,
                },
            )
        })
        .collect();
    wallet.persist(&mut conn).unwrap();
    outpoints
}

pub fn json_stdout(assert: assert_cmd::assert::Assert) -> serde_json::Value {
    serde_json::from_slice(&assert.get_output().stdout).expect("json output")
}
