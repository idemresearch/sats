//! Human approval integration: real terminal input against disposable wallets.
#![cfg(all(unix, feature = "mcp"))]

mod common;

use std::process::Command;

#[test]
fn terminal_selection_review_and_recovery() {
    let dir = tempfile::TempDir::new().unwrap();
    let binary = std::env::var_os("SATS_TEST_BIN")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_sats").into());
    let init = Command::new(&binary)
        .arg("init")
        .env("SATS_DIR", dir.path())
        .env("SATS_PASSWORD", common::PASSWORD)
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    common::write_mock_provider(&dir);
    common::fund_wallet(&dir, &[100_000]);
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/human_review_pty.py"
        ))
        .arg(binary)
        .arg(dir.path())
        .output()
        .expect("human terminal tests require python3 (standard library only)");
    assert!(
        output.status.success(),
        "PTY scenarios failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
