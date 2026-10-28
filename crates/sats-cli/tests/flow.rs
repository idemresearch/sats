//! End-to-end spend flows against a funded, persisted wallet and the
//! hermetic mock provider: confirmed sends and dry runs.

mod common;

use common::{ADDRESS, fund_wallet, init_wallet, json_stdout, sats, write_mock_provider};
use std::fs;
use tempfile::TempDir;

#[test]
fn send_broadcasts_and_status_history_see_it() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    fund_wallet(&dir, &[100_000]);

    let json = json_stdout(
        sats(&dir)
            .args([
                "send",
                ADDRESS,
                "25000",
                "--fee-rate",
                "2",
                "--yes",
                "--json",
            ])
            .assert()
            .success(),
    );
    let txid = json["txid"].as_str().expect("txid").to_string();
    assert_eq!(json["amount_sat"], 25_000);
    assert!(json["fee_sat"].as_u64().unwrap() > 0);

    // The mock provider records the broadcast.
    let log = fs::read_to_string(mockdata.join("broadcasts.log")).unwrap();
    assert!(log.contains(&txid));

    // status: broadcast (the wallet sees it in the mempool), nothing pending.
    let status = json_stdout(sats(&dir).args(["status", "--json"]).assert().success());
    assert_eq!(status["pending"], serde_json::json!([]));
    assert_eq!(status["broadcast"][0]["txid"], txid.as_str());
    assert_eq!(status["broadcast"][0]["seen"], "mempool");

    // history: the unconfirmed send first (recipient enriched from the
    // record), then the confirmed funding receive.
    let history = json_stdout(
        sats(&dir)
            .args(["history", "--offline", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(history.as_array().unwrap().len(), 2);
    assert_eq!(history[0]["txid"], txid.as_str());
    assert_eq!(history[0]["direction"], "sent");
    assert_eq!(history[0]["status"], "unconfirmed");
    assert_eq!(history[0]["recipient"], ADDRESS);
    assert_eq!(history[1]["direction"], "received");
    assert_eq!(history[1]["status"], "confirmed");
    assert_eq!(history[1]["net_sat"], 100_000);

    // Repeating recovery repairs local receipts without another network broadcast.
    sats(&dir)
        .args(["tx", "broadcast", &txid])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(mockdata.join("broadcasts.log")).unwrap(),
        log
    );
}

#[test]
fn dry_run_persists_nothing_and_needs_no_password() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    write_mock_provider(&dir);
    fund_wallet(&dir, &[100_000]);

    let json = json_stdout(
        sats(&dir)
            .env_remove("SATS_PASSWORD")
            .args([
                "send",
                ADDRESS,
                "25000",
                "--fee-rate",
                "2",
                "--dry-run",
                "--json",
            ])
            .assert()
            .success(),
    );
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["amount_sat"], 25_000);
    assert!(json["fee_sat"].as_u64().unwrap() > 0);

    // Nothing was signed or saved.
    assert!(!dir.path().join("signet/transactions").exists());
    assert!(!dir.path().join("signet/psbts").exists());

    // A real send still works afterwards.
    sats(&dir)
        .args([
            "send",
            ADDRESS,
            "25000",
            "--fee-rate",
            "2",
            "--yes",
            "--json",
        ])
        .assert()
        .success();
}
