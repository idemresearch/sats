//! Alkanes CLI integration tests: the view commands against the
//! file-driven mock provider, and the resolution failure modes.

mod common;

use common::{init_wallet, json_stdout, sats, write_mock_provider};
use predicates::prelude::*;
use tempfile::TempDir;

#[test]
fn inspect_hashes_the_mock_bytecode() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    std::fs::write(
        mockdata.join("alkanes-bytecode.json"),
        r#"{"2:1": "0x0061736d"}"#,
    )
    .unwrap();

    let inspected = json_stdout(
        sats(&dir)
            .args(["alkanes", "inspect", "2:1", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(inspected["id"], "2:1");
    assert_eq!(inspected["bytecode_bytes"], 4);
    assert_eq!(
        inspected["code_hash"],
        // sha256 of 0061736d ("\0asm").
        "cd5d4935a48c0672cb06407bb443bc0087aff947c6b864bac886982c73b3027f"
    );

    // An id nothing is deployed at is a typed view error.
    sats(&dir)
        .args(["alkanes", "inspect", "2:9"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no bytecode for 2:9"));

    // Garbage ids fail before any provider access.
    sats(&dir)
        .args(["alkanes", "inspect", "not-an-id"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid alkane id"));
}

#[test]
fn simulate_shows_parsed_fields_and_the_raw_result() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    std::fs::write(
        mockdata.join("alkanes-simulate.json"),
        r#"{"status": 0, "gasUsed": 82500,
            "execution": {"alkanes": [{"id": {"block": 2, "tx": 1}, "value": "1000"}]},
            "unrecognized": {"detail": true}}"#,
    )
    .unwrap();

    let simulated = json_stdout(
        sats(&dir)
            .args(["alkanes", "simulate", "2:1", "77", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(simulated["id"], "2:1");
    assert_eq!(simulated["inputs"][0], "77");
    assert_eq!(simulated["status"], 0);
    assert_eq!(simulated["gas_used"], 82_500);
    assert_eq!(simulated["transfers"][0]["value"], 1_000);
    // The raw result is preserved verbatim, unknown fields included.
    assert_eq!(simulated["raw"]["unrecognized"]["detail"], true);

    // A missing simulation fixture fails closed with a typed error.
    std::fs::remove_file(mockdata.join("alkanes-simulate.json")).unwrap();
    sats(&dir)
        .args(["alkanes", "simulate", "2:1", "77"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("alkanes view failed"));
}

#[test]
fn unconfigured_view_is_a_typed_resolution_error() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    // No Subfrost set up: the default chain source serves no views.
    sats(&dir)
        .args(["alkanes", "inspect", "2:1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Alkanes views on signet need Subfrost",
        ))
        .stderr(predicate::str::contains("sats providers add subfrost"));
}

#[test]
#[cfg(feature = "experimental-alkanes-execute")]
fn execute_composes_signs_and_broadcasts_the_runestone() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    std::fs::write(mockdata.join("alkanes-simulate.json"), r#"{"status": 0}"#).unwrap();
    common::fund_wallet(&dir, &[100_000]);

    let out = json_stdout(
        sats(&dir)
            .args(["alkanes", "execute", "2:1", "77", "-y", "--json"])
            .assert()
            .success(),
    );
    let txid = out["txid"].as_str().unwrap().to_string();
    assert_eq!(out["target"], "2:1");
    assert_eq!(out["postage_sat"], 546);

    // The broadcast reached the mock chain.
    let broadcasts = std::fs::read_to_string(mockdata.join("broadcasts.log")).unwrap();
    assert!(broadcasts.contains(&txid));

    // The durable record is an ordinary attributed CLI transaction.
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            dir.path()
                .join("signet/transactions")
                .join(format!("{txid}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(record["recipient"], "alkanes:2:1");
    assert_eq!(record["status"], "broadcast");
    assert_eq!(record["origin"]["surface"], "cli");

    // Decode the signed transaction: output 0 is exactly the runestone
    // the pure crate encodes for this call, output 1 the postage.
    let tx: sats_core::bitcoin::Transaction = sats_core::bitcoin::consensus::deserialize(
        &hex::decode(record["tx_hex"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    let expected =
        sats_alkanes::protostone::runestone_script(&[sats_alkanes::protostone::Protostone {
            protocol_tag: sats_alkanes::protostone::ALKANES_PROTOCOL_TAG,
            message: sats_alkanes::call::AlkaneCall {
                target: sats_alkanes::id::AlkaneId { block: 2, tx: 1 },
                inputs: vec![77],
            }
            .encode_cellpack(),
            pointer: Some(1),
            refund_pointer: Some(1),
        }])
        .unwrap();
    assert_eq!(tx.output[0].script_pubkey, expected);
    assert_eq!(tx.output[0].value.to_sat(), 0);
    assert_eq!(tx.output[1].value.to_sat(), 546);
    assert!(
        tx.output[1].script_pubkey.is_p2tr(),
        "postage pays the wallet"
    );
}

#[test]
#[cfg(feature = "experimental-alkanes-execute")]
fn execute_refuses_mainnet() {
    let dir = TempDir::new().unwrap();
    sats(&dir)
        .args([
            "--network",
            "mainnet",
            "alkanes",
            "execute",
            "2:1",
            "77",
            "-y",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not enabled on mainnet"));
}

#[test]
#[cfg(feature = "experimental-alkanes-execute")]
fn execute_fails_closed_without_simulation() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    write_mock_provider(&dir);
    common::fund_wallet(&dir, &[100_000]);
    // No alkanes-simulate.json: the call is never composed.
    sats(&dir)
        .args(["alkanes", "execute", "2:1", "77", "-y"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("alkanes view failed"));
    assert!(!dir.path().join("signet/transactions").exists());
}

#[test]
#[cfg(feature = "experimental-alkanes-execute")]
fn execute_excludes_dust_suspects() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    std::fs::write(mockdata.join("alkanes-simulate.json"), r#"{"status": 0}"#).unwrap();
    common::fund_wallet(&dir, &[100_000, 546]);

    let out = json_stdout(
        sats(&dir)
            .args(["alkanes", "execute", "2:1", "77", "-y", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(out["excluded_utxos"], 1, "the 546-sat suspect stays out");
}

#[test]
#[cfg(not(feature = "experimental-alkanes-execute"))]
fn default_release_has_no_alkanes_execution_command() {
    let dir = TempDir::new().unwrap();
    sats(&dir)
        .args(["alkanes", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("inspect"))
        .stdout(predicate::str::contains("simulate"))
        .stdout(predicate::str::contains("execute").not());
    sats(&dir)
        .args(["alkanes", "execute", "2:1", "77", "-y"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unrecognized subcommand"));
    assert!(!dir.path().join("seed.sealed").exists());
    assert!(!dir.path().join("signet").exists());
}
