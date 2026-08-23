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
    // No providers configured: the built-in esplora fallback covers chain
    // capabilities but never alkanes.view.
    sats(&dir)
        .args(["alkanes", "inspect", "2:1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no alkanes.view provider"))
        .stderr(predicate::str::contains("[providers]"));
}

#[test]
fn ambiguous_view_providers_are_rejected() {
    let dir = TempDir::new().unwrap();
    init_wallet(&dir);
    let mockdata = dir.path().join("mockdata");
    std::fs::create_dir_all(&mockdata).unwrap();
    let config = format!(
        concat!(
            "network = \"signet\"\n\n",
            "[providers.chain]\ndriver = \"mock\"\nnetwork = \"signet\"\n",
            "url = \"file://{dir}\"\ncapabilities = [\"chain\"]\n\n",
            "[providers.viewa]\ndriver = \"mock\"\nnetwork = \"signet\"\n",
            "url = \"file://{dir}\"\ncapabilities = [\"alkanes.view\"]\n\n",
            "[providers.viewb]\ndriver = \"mock\"\nnetwork = \"signet\"\n",
            "url = \"file://{dir}\"\ncapabilities = [\"alkanes.view\"]\n",
        ),
        dir = mockdata.display()
    );
    std::fs::write(dir.path().join("config.toml"), config).unwrap();

    sats(&dir)
        .args(["alkanes", "inspect", "2:1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("multiple alkanes.view providers"));
}
