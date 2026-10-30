//! Funds the disposable signet wallet that the launch video's terminal
//! scenes are recorded against. Ignored by default:
//! `video/scripts/capture.py` runs it after `sats init`, then records the
//! real binary on a terminal against the file-driven mock chain.

mod common;

/// `SATS_VIDEO_DIR` is the wallet's data directory and `SATS_VIDEO_FUND`
/// the confirmed output values in sats, comma-separated. Prints each
/// funded outpoint as `funded TXID:VOUT`, in order.
#[test]
#[ignore = "run by video/scripts/capture.py"]
fn fund_video_wallet() {
    let dir = std::env::var_os("SATS_VIDEO_DIR").expect("SATS_VIDEO_DIR");
    let values: Vec<u64> = std::env::var("SATS_VIDEO_FUND")
        .expect("SATS_VIDEO_FUND")
        .split(',')
        .map(|value| value.trim().parse().expect("a value in sats"))
        .collect();
    let db = std::path::Path::new(&dir).join("signet/wallet.sqlite");
    for outpoint in common::fund_wallet_db(&db, &values) {
        println!("funded {outpoint}");
    }
}
