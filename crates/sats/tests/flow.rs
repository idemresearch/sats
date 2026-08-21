//! End-to-end spend flows against a funded, persisted wallet and the
//! hermetic mock provider: automatic sends, dry runs, the explicit
//! PSBT/tx escape hatch, and legacy-state compatibility.

mod common;

use std::fs;
use std::str::FromStr;

use bdk_wallet::bitcoin::{Address, Amount, FeeRate, Network};
use common::{ADDRESS, fund_wallet, init_wallet, json_stdout, sats, write_mock_provider};
use predicates::prelude::*;
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

    // A broadcast transaction cannot be re-broadcast by id.
    sats(&dir)
        .args(["tx", "broadcast", &txid])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already broadcast"));
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

#[test]
fn export_inspect_sign_broadcast_roundtrip() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    fund_wallet(&dir, &[100_000]);
    let psbt_path = dir.path().join("spend.psbt");
    let psbt_arg = psbt_path.to_str().unwrap();

    // Export needs no password: nothing is signed.
    let json = json_stdout(
        sats(&dir)
            .env_remove("SATS_PASSWORD")
            .args([
                "send",
                ADDRESS,
                "25000",
                "--fee-rate",
                "2",
                "--export-psbt",
                psbt_arg,
                "--json",
            ])
            .assert()
            .success(),
    );
    assert_eq!(json["psbt_file"], psbt_arg);
    assert!(psbt_path.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&psbt_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let inspect = json_stdout(
        sats(&dir)
            .args(["psbt", "inspect", psbt_arg, "--json"])
            .assert()
            .success(),
    );
    assert_eq!(inspect["finalized"], false);
    assert_eq!(inspect["signed_inputs"], 0);
    assert!(inspect["fee_sat"].as_u64().unwrap() > 0);
    let outputs = inspect["outputs"].as_array().unwrap();
    assert!(
        outputs
            .iter()
            .any(|o| o["address"] == ADDRESS && o["value_sat"] == 25_000)
    );

    let signed = json_stdout(
        sats(&dir)
            .args(["psbt", "sign", psbt_arg, "--json"])
            .assert()
            .success(),
    );
    assert_eq!(signed["status"], "signed");
    assert_eq!(signed["recipient"], ADDRESS);
    assert_eq!(signed["amount_sat"], 25_000);
    let txid = signed["txid"].as_str().unwrap().to_string();

    let status = json_stdout(
        sats(&dir)
            .args(["status", "--offline", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(status["pending"][0]["txid"], txid.as_str());

    let broadcast = json_stdout(
        sats(&dir)
            .args(["tx", "broadcast", &txid, "--json"])
            .assert()
            .success(),
    );
    assert_eq!(broadcast["txid"], txid.as_str());
    let log = fs::read_to_string(mockdata.join("broadcasts.log")).unwrap();
    assert!(log.contains(&txid));

    // The raw hex file path broadcasts too (idempotent at the provider).
    let record: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(dir.path().join(format!("signet/transactions/{txid}.json"))).unwrap(),
    )
    .unwrap();
    let hex_path = dir.path().join("raw.hex");
    fs::write(&hex_path, record["tx_hex"].as_str().unwrap()).unwrap();
    sats(&dir)
        .args(["tx", "broadcast", hex_path.to_str().unwrap(), "--json"])
        .assert()
        .success();
}

#[test]
fn psbt_sign_out_writes_artifact_only() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    write_mock_provider(&dir);
    fund_wallet(&dir, &[100_000]);
    let psbt_path = dir.path().join("spend.psbt");
    let signed_path = dir.path().join("signed.psbt");

    sats(&dir)
        .args([
            "send",
            ADDRESS,
            "25000",
            "--fee-rate",
            "2",
            "--export-psbt",
            psbt_path.to_str().unwrap(),
        ])
        .assert()
        .success();
    let json = json_stdout(
        sats(&dir)
            .args([
                "psbt",
                "sign",
                psbt_path.to_str().unwrap(),
                "--out",
                signed_path.to_str().unwrap(),
                "--json",
            ])
            .assert()
            .success(),
    );
    assert_eq!(json["finalized"], true);
    assert!(signed_path.exists());
    // Artifact only: no pending transaction was staged.
    assert!(!dir.path().join("signet/transactions").exists());
}

#[test]
fn legacy_psbt_session_signs_by_explicit_id() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    write_mock_provider(&dir);
    fund_wallet(&dir, &[100_000]);

    // Fabricate the stored session an older release would have written.
    let session = build_session(&dir);
    let session_path = dir.path().join(format!(
        "signet/psbts/{}.json",
        session["id"].as_str().unwrap()
    ));
    fs::create_dir_all(session_path.parent().unwrap()).unwrap();
    fs::write(&session_path, serde_json::to_vec_pretty(&session).unwrap()).unwrap();
    let id = session["id"].as_str().unwrap();

    let signed = json_stdout(
        sats(&dir)
            .args(["psbt", "sign", "--session", id, "--json"])
            .assert()
            .success(),
    );
    assert_eq!(signed["status"], "signed");
    assert_eq!(signed["id"], id);
    assert!(!session_path.exists(), "session must be consumed");

    // The session id still resolves the saved transaction.
    sats(&dir)
        .args(["tx", "broadcast", id, "--json"])
        .assert()
        .success();
}

#[test]
fn legacy_unsigned_plan_signs_by_explicit_id() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    write_mock_provider(&dir);
    fund_wallet(&dir, &[100_000]);

    let session = build_session(&dir);
    let legacy = serde_json::json!({
        "id": "legacyplan",
        "network": "signet",
        "recipient": session["recipient"],
        "amount_sat": session["amount_sat"],
        "fee_sat": session["fee_sat"],
        "created_at": session["created_at"],
        "status": "unsigned",
        "psbt": session["psbt"],
        "excluded_utxos": 0,
    });
    let legacy_path = dir.path().join("signet/plans/legacyplan.json");
    fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
    fs::write(&legacy_path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

    let signed = json_stdout(
        sats(&dir)
            .args(["psbt", "sign", "--session", "legacyplan", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(signed["status"], "signed");
    assert!(!legacy_path.exists(), "legacy plan must be consumed");
    sats(&dir)
        .args(["tx", "broadcast", "legacyplan", "--json"])
        .assert()
        .success();
}

/// Build a PSBT session JSON the way an older release's staged workflow
/// would have persisted it, using the same engine the CLI uses.
fn build_session(dir: &TempDir) -> serde_json::Value {
    let db = dir.path().join("signet/wallet.sqlite");
    let mut conn = rusqlite::Connection::open(&db).unwrap();
    let mut wallet = bdk_wallet::Wallet::load()
        .check_network(Network::Signet)
        .load_wallet(&mut conn)
        .unwrap()
        .expect("wallet exists");
    let addr = Address::from_str(ADDRESS)
        .unwrap()
        .require_network(Network::Signet)
        .unwrap();
    let prepared = sats_core::engine::build_plan(
        &mut wallet,
        &addr,
        Amount::from_sat(25_000),
        FeeRate::from_sat_per_vb_u32(2),
        &[],
        "signet",
        42,
    )
    .unwrap();
    wallet.persist(&mut conn).unwrap();
    serde_json::to_value(prepared.session()).unwrap()
}
