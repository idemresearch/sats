//! Alkanes CLI integration tests: views, human transfers, asset protection,
//! and provider failure modes against the file-driven mock provider.

mod common;

use common::sats;
use common::{init_wallet, json_stdout, write_mock_provider};
use predicates::prelude::*;
use std::str::FromStr;
use tempfile::TempDir;

#[test]
fn standard_build_includes_all_alkanes_commands() {
    let dir = TempDir::new().unwrap();
    sats(&dir)
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("alkanes"));
    sats(&dir)
        .args(["alkanes", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("frbtc"))
        .stdout(predicate::str::contains("balance"))
        .stdout(predicate::str::contains("inspect"))
        .stdout(predicate::str::contains("simulate"))
        .stdout(predicate::str::contains("send"));
    // There is no generic contract call: sats signs only operations it
    // composes and verifies itself.
    sats(&dir)
        .args(["alkanes", "execute", "2:1", "77", "-y"])
        .assert()
        .code(2);
    assert!(!dir.path().join("seed.sealed").exists());
    assert!(!dir.path().join("signet").exists());
}

#[test]
fn frbtc_info_is_read_only_and_obsolete_names_and_grant_flags_are_removed() {
    let dir = TempDir::new().unwrap();
    let mock = write_mock_provider(&dir);
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
    let info = json_stdout(
        sats(&dir)
            .args(["alkanes", "frbtc", "info", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(info["id"], "32:0");
    assert_eq!(info["network"], "signet");
    assert!(info["signer_address"].as_str().unwrap().starts_with("tb1p"));
    sats(&dir).args(["alkanes", "wrap-info"]).assert().failure();
    sats(&dir)
        .args(["agent", "grant", "claude", "--allow-alkanes-wrap"])
        .assert()
        .failure();
    assert!(!dir.path().join("seed.sealed").exists());
    assert!(!dir.path().join("signet").exists());
}

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
fn alkanes_need_subfrost_as_the_provider() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    // The default provider serves no views.
    let refused = |dir: &TempDir| {
        sats(dir)
            .args(["alkanes", "inspect", "2:1"])
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "Alkanes on signet needs Subfrost as the provider",
            ))
            .stderr(predicate::str::contains("sats providers add subfrost"));
    };
    refused(&dir);
    // A saved key alone isn't enough: Subfrost must be the provider.
    std::fs::write(
        dir.path().join("config.toml"),
        "network = \"signet\"\n\n[subfrost]\napi_key = \"k\"\n",
    )
    .unwrap();
    refused(&dir);
}

#[test]
fn alkanes_send_json_refuses_nonterminal_confirmation_without_unlocking() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    let funded = common::fund_wallet(&dir, &[100_000]);
    mark_outputs(&mockdata, &[(funded[0], Some(&[("32:0", "5000")]))]);
    sats(&dir)
        .args(["alkanes", "send", "32:0", "1000", common::ADDRESS, "--json"])
        .env("SATS_PASSWORD", "wrong-password")
        .assert()
        .failure()
        .stdout("")
        .stderr(predicate::str::contains("Send"))
        .stderr(predicate::str::contains("32:0"))
        .stderr(predicate::str::contains("Fee"))
        .stderr(predicate::str::contains("Total"))
        .stderr(predicate::str::contains(
            "cannot confirm: stdin is not a terminal",
        ));
    assert!(!mockdata.join("broadcasts.log").exists());
}

#[test]
#[cfg(unix)]
fn alkanes_send_review_precedes_password_on_a_terminal() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    let funded = common::fund_wallet(&dir, &[100_000]);
    mark_outputs(&mockdata, &[(funded[0], Some(&[("32:0", "5000")]))]);
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/alkanes_review_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_sats"))
        .arg(dir.path())
        .output()
        .expect("terminal tests require python3 (standard library only)");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

type MockOutput<'a> = (
    bdk_wallet::bitcoin::OutPoint,
    Option<&'a [(&'a str, &'a str)]>,
);

/// Write the mock indexer's view of wallet outputs: `Some(entries)` for
/// an output carrying alkanes, `None` for one the indexer doesn't know.
fn mark_outputs(mockdata: &std::path::Path, outputs: &[MockOutput<'_>]) {
    let map: serde_json::Map<String, serde_json::Value> = outputs
        .iter()
        .map(|(outpoint, entries)| {
            let value = match entries {
                None => serde_json::Value::Null,
                Some(entries) => entries
                    .iter()
                    .map(|(id, amount)| (id.to_string(), serde_json::json!(amount)))
                    .collect::<serde_json::Map<_, _>>()
                    .into(),
            };
            (outpoint.to_string(), value)
        })
        .collect();
    std::fs::write(
        mockdata.join("alkanes-outpoints.json"),
        serde_json::Value::Object(map).to_string(),
    )
    .unwrap();
}

/// The one transaction the CLI saved.
fn saved_transaction(dir: &TempDir) -> sats_core::bitcoin::Transaction {
    let entries: Vec<_> = std::fs::read_dir(dir.path().join("signet/transactions"))
        .unwrap()
        .collect();
    assert_eq!(entries.len(), 1, "exactly one saved transaction");
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(entries[0].as_ref().unwrap().path()).unwrap())
            .unwrap();
    sats_core::bitcoin::consensus::deserialize(
        &hex::decode(record["tx_hex"].as_str().unwrap()).unwrap(),
    )
    .unwrap()
}

/// Write the labels the mock indexer reports for each asset id.
fn label_assets(mockdata: &std::path::Path, labels: &[(&str, &str, &str)]) {
    let map: serde_json::Map<String, serde_json::Value> = labels
        .iter()
        .map(|(id, name, symbol)| {
            (
                id.to_string(),
                serde_json::json!({"name": name, "symbol": symbol}),
            )
        })
        .collect();
    std::fs::write(
        mockdata.join("alkanes-names.json"),
        serde_json::Value::Object(map).to_string(),
    )
    .unwrap();
}

#[test]
fn labels_are_unverified_and_an_frbtc_impostor_is_flagged() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    let funded = common::fund_wallet(&dir, &[546, 546, 100_000]);
    mark_outputs(
        &mockdata,
        &[
            (funded[0], Some(&[("32:0", "5000")])),
            (funded[1], Some(&[("2:9", "7")])),
        ],
    );
    // 2:9 chose frBTC's name for itself.
    label_assets(
        &mockdata,
        &[("32:0", "frBTC", "FRBTC"), ("2:9", "frBTC", "FRBTC")],
    );
    let recipient = common::foreign_address(bdk_wallet::bitcoin::Network::Signet);
    let impostor = "2:9 calls itself frBTC, but frBTC is 32:0";

    sats(&dir)
        .args(["alkanes", "balance"])
        .assert()
        .success()
        .stdout(predicate::str::contains("frBTC (FRBTC)"))
        .stdout(predicate::str::contains("indexer's labels, not verified"))
        .stdout(predicate::str::contains(format!("! {impostor}")))
        .stdout(predicate::str::contains("32:0 calls itself").not());
    // Labels stay plain data in JSON.
    let out = json_stdout(
        sats(&dir)
            .args(["alkanes", "balance", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(out["assets"][0]["id"], "2:9");
    assert_eq!(out["assets"][0]["name"], "frBTC");

    // Enter alone does not sign the impostor.
    #[cfg(unix)]
    {
        let output = std::process::Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/alkanes_impostor_pty.py"
            ))
            .arg(env!("CARGO_BIN_EXE_sats"))
            .arg(dir.path())
            .arg("2:9")
            .arg(&recipient)
            .output()
            .expect("terminal tests require python3 (standard library only)");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // The review names the label as the indexer's and warns, with --json on stderr.
    sats(&dir)
        .args([
            "alkanes",
            "send",
            "2:9",
            "7",
            &recipient,
            "--fee-rate",
            "2",
            "-y",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"asset\":\"2:9\""))
        .stdout(predicate::str::contains("calls itself").not())
        .stderr(predicate::str::contains("Send     7 of 2:9"))
        .stderr(predicate::str::contains(
            "Label    frBTC (FRBTC), the indexer's, not verified",
        ))
        .stderr(predicate::str::contains(format!("! {impostor}")));

    // The real frBTC carries the same label without a warning.
    sats(&dir)
        .args([
            "alkanes",
            "send",
            "32:0",
            "1000",
            &recipient,
            "--fee-rate",
            "2",
            "-y",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Send     1000 of 32:0"))
        .stdout(predicate::str::contains(
            "Label    frBTC (FRBTC), the indexer's, not verified",
        ))
        .stdout(predicate::str::contains("calls itself").not());
}

#[test]
fn balance_reports_assets_and_unindexed_outputs() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    let funded = common::fund_wallet(&dir, &[546, 546, 100_000, 20_000]);
    mark_outputs(
        &mockdata,
        &[
            (funded[0], Some(&[("32:0", "100000"), ("2:9", "7")])),
            (funded[1], Some(&[("32:0", "50000")])),
            (funded[3], None),
        ],
    );

    let out = json_stdout(
        sats(&dir)
            .args(["alkanes", "balance", "--json"])
            .assert()
            .success(),
    );
    let assets = out["assets"].as_array().unwrap();
    assert_eq!(assets.len(), 2);
    assert_eq!(assets[0]["id"], "2:9");
    assert_eq!(assets[0]["amount"], "7");
    assert_eq!(assets[1]["id"], "32:0");
    assert_eq!(assets[1]["amount"], "150000");
    assert_eq!(out["outputs"].as_array().unwrap().len(), 2);
    assert_eq!(out["unindexed_outputs"][0], funded[3].to_string());

    // A provider without Alkanes views refuses rather than reporting none.
    std::fs::write(
        dir.path().join("config.toml"),
        "network = \"signet\"\n\n[signet]\nchain = \"mempool\"\n",
    )
    .unwrap();
    sats(&dir)
        .args(["alkanes", "balance"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("needs Subfrost as the provider"));
}

#[test]
fn a_plain_send_never_spends_outputs_carrying_or_unproven() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    // Values the dust heuristic doesn't catch: only the indexer can.
    let funded = common::fund_wallet(&dir, &[60_000, 70_000, 100_000]);
    mark_outputs(
        &mockdata,
        &[(funded[0], Some(&[("32:0", "1")])), (funded[1], None)],
    );
    let send = |amount: &str| {
        let mut cmd = sats(&dir);
        cmd.args([
            "send",
            &common::foreign_address(bdk_wallet::bitcoin::Network::Signet),
            amount,
            "--fee-rate",
            "2",
            "--yes",
        ]);
        cmd
    };

    // More than the one clean output holds: the others are off limits.
    send("120000")
        .assert()
        .failure()
        .stderr(predicate::str::contains("nsufficient"));

    send("50000").assert().success();
    let tx = saved_transaction(&dir);
    let spent: Vec<String> = tx
        .input
        .iter()
        .map(|i| i.previous_output.to_string())
        .collect();
    assert_eq!(spent, vec![funded[2].to_string()]);
}

#[test]
fn a_plain_send_fails_closed_without_the_indexer() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    common::fund_wallet(&dir, &[100_000]);
    // No alkanes-outpoints.json: the indexer can't answer.
    std::fs::remove_file(mockdata.join("alkanes-outpoints.json")).unwrap();
    sats(&dir)
        .args([
            "send",
            &common::foreign_address(bdk_wallet::bitcoin::Network::Signet),
            "50000",
            "--fee-rate",
            "2",
            "--yes",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("alkanes view failed"));
    assert!(!dir.path().join("signet/transactions").exists());
}

#[test]
fn alkanes_send_moves_the_exact_amount_and_keeps_the_rest() {
    use sats_alkanes::decode::transaction_protostones;
    use sats_alkanes::id::AlkaneId;
    use sats_alkanes::protostone::Edict;

    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    let funded = common::fund_wallet(&dir, &[546, 100_000, 30_000]);
    mark_outputs(
        &mockdata,
        &[
            (funded[0], Some(&[("32:0", "100000"), ("2:9", "7")])),
            // Carries another asset: must not fund the fee.
            (funded[2], Some(&[("2:9", "1")])),
        ],
    );
    let recipient = common::foreign_address(bdk_wallet::bitcoin::Network::Signet);

    let out = json_stdout(
        sats(&dir)
            .args([
                "alkanes",
                "send",
                "32:0",
                "40000",
                &recipient,
                "--fee-rate",
                "2",
                "-y",
                "--json",
            ])
            .assert()
            .success()
            // The review precedes signing even with --json, on stderr.
            .stderr(predicate::str::contains("40000 of 32:0"))
            .stderr(predicate::str::contains(&recipient)),
    );
    assert_eq!(out["asset"], "32:0");
    assert_eq!(out["amount"], "40000");
    assert_eq!(out["postage_sat"], 546);
    let kept = out["kept"].as_array().unwrap();
    assert!(kept.contains(&serde_json::json!({"id": "32:0", "amount": "60000"})));
    assert!(kept.contains(&serde_json::json!({"id": "2:9", "amount": "7"})));
    let broadcasts = std::fs::read_to_string(mockdata.join("broadcasts.log")).unwrap();
    assert!(broadcasts.contains(out["txid"].as_str().unwrap()));

    // The signed transaction, decoded independently.
    let tx = saved_transaction(&dir);
    let spent: Vec<String> = tx
        .input
        .iter()
        .map(|i| i.previous_output.to_string())
        .collect();
    assert!(
        spent.contains(&funded[0].to_string()),
        "the carrier is spent"
    );
    assert!(
        !spent.contains(&funded[2].to_string()),
        "the other carrier stays"
    );
    let (vout, stones) = transaction_protostones(&tx).unwrap();
    assert_eq!(vout, 0);
    assert_eq!(stones.len(), 1);
    assert!(stones[0].message.is_empty());
    assert_eq!(stones[0].pointer, Some(2));
    assert_eq!(
        stones[0].edicts,
        vec![Edict {
            id: AlkaneId { block: 32, tx: 0 },
            amount: 40_000,
            output: 1,
        }]
    );
    assert_eq!(
        tx.output[1].script_pubkey.to_hex_string(),
        bdk_wallet::bitcoin::Address::from_str(&recipient)
            .unwrap()
            .assume_checked()
            .script_pubkey()
            .to_hex_string()
    );
}

#[test]
fn alkanes_send_refuses_what_the_wallet_cannot_prove_it_holds() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = write_mock_provider(&dir);
    let funded = common::fund_wallet(&dir, &[546, 546, 100_000]);
    mark_outputs(
        &mockdata,
        &[
            (funded[0], Some(&[("32:0", "100000")])),
            // Unindexed: whatever it holds can't be counted.
            (funded[1], None),
        ],
    );
    let recipient = common::foreign_address(bdk_wallet::bitcoin::Network::Signet);
    let send = |amount: &str| {
        let mut cmd = sats(&dir);
        cmd.args([
            "alkanes",
            "send",
            "32:0",
            amount,
            &recipient,
            "--fee-rate",
            "2",
            "-y",
        ]);
        cmd
    };
    send("100001")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "holds 100000 of 32:0 in indexed outputs",
        ));
    send("0")
        .assert()
        .failure()
        .stderr(predicate::str::contains("amount must be positive"));
    assert!(!dir.path().join("signet/transactions").exists());
}
