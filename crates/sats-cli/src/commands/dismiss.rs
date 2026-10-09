//! `sats agent dismiss` — decline a pending request without executing it.
//!
//! Reducing authority needs no password — the same posture as
//! `sats agent revoke`.

use anyhow::Result;
use sats_core::bitcoin::Network;
use sats_wallet::request;
use sats_wallet::store::Store;

use crate::ui;

pub fn run(store: &Store, network: Network, id_or_prefix: &str, json: bool) -> Result<()> {
    let dismissed = request::dismiss(store, network, id_or_prefix)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": dismissed.id,
                "status": dismissed.status(),
            })
        );
    } else {
        ui::ok(&format!("dismissed  {}", dismissed.id));
    }
    Ok(())
}
