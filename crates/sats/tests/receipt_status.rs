//! Receipt and chain observation regressions, using only disposable wallets.
mod common;

use bdk_wallet::bitcoin::{BlockHash, Network, Txid, hashes::Hash};
use bdk_wallet::chain::{BlockId, ConfirmationBlockTime};
use common::{ADDRESS, fund_wallet, init_wallet, json_stdout, sats, write_mock_provider};
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

#[test]
fn pending_saved_transactions_keep_chain_observations_and_report_sync_freshness() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    fund_wallet(&dir, &[100_000]);
    let sent = json_stdout(
        sats(&dir)
            .args([
                "send",
                ADDRESS,
                "10000",
                "--fee-rate",
                "2",
                "--yes",
                "--json",
            ])
            .assert()
            .success(),
    );
    let txid = sent["txid"].as_str().unwrap();
    // The network accepted this transaction; local bookkeeping was lost.
    let path = dir.path().join(format!("signet/transactions/{txid}.json"));
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["status"] = "pending".into();
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let list = json_stdout(sats(&dir).args(["status", "--json"]).assert().success());
    assert_eq!(list["pending"][0]["seen"], "mempool");
    assert_eq!(list["synced"], true);
    assert_eq!(list["sync_status"], "fresh");
    let detail = json_stdout(
        sats(&dir)
            .args(["status", txid, "--offline", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(detail["seen"], "mempool");
    assert_eq!(detail["synced"], false);
    assert_eq!(detail["sync_status"], "offline");
    sats(&dir)
        .args(["status", txid, "--offline"])
        .assert()
        .success()
        .stdout(predicate::str::contains("signed; broadcast unconfirmed"))
        .stdout(predicate::str::contains("in mempool"));

    let mut conn = rusqlite::Connection::open(dir.path().join("signet/wallet.sqlite")).unwrap();
    let mut wallet = bdk_wallet::Wallet::load()
        .check_network(Network::Signet)
        .load_wallet(&mut conn)
        .unwrap()
        .unwrap();
    let block = BlockId {
        height: 1_001,
        hash: BlockHash::all_zeros(),
    };
    bdk_wallet::test_utils::insert_checkpoint(&mut wallet, block);
    bdk_wallet::test_utils::insert_anchor(
        &mut wallet,
        txid.parse::<Txid>().unwrap(),
        ConfirmationBlockTime {
            block_id: block,
            confirmation_time: 200,
        },
    );
    wallet.persist(&mut conn).unwrap();
    drop(conn);
    fs::write(mockdata.join("sync-error"), "fixture offline").unwrap();
    let stale = json_stdout(sats(&dir).args(["status", "--json"]).assert().success());
    assert_eq!(stale["pending"][0]["seen"], "confirmed");
    assert_eq!(stale["pending"][0]["confirmations"], 1);
    assert_eq!(stale["synced"], false);
    assert_eq!(stale["sync_status"], "failed");
    let stale_detail = json_stdout(
        sats(&dir)
            .args(["status", txid, "--json"])
            .assert()
            .success(),
    );
    assert_eq!(stale_detail["seen"], "confirmed");
    assert_eq!(stale_detail["synced"], false);
    assert_eq!(stale_detail["sync_status"], "failed");
}
