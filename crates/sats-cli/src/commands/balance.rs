use anyhow::Result;
use sats_core::bitcoin::Network;

use crate::provider::Services;
use crate::store::Store;
use crate::{ui, walletd};

pub fn run(
    store: &Store,
    network: Network,
    resolve_services: impl FnOnce() -> Result<Services>,
    offline: bool,
    json: bool,
) -> Result<()> {
    let mut ctx = walletd::open(store, network)?;
    let mut synced = false;
    if !offline {
        let services = resolve_services()?;
        match services.sync_wallet(&mut ctx) {
            Ok(()) => synced = true,
            Err(err) => eprintln!("✗ sync failed — showing cached balance ({err:#})"),
        }
    }
    let balance = ctx.wallet.balance();
    let spendable = (balance.confirmed + balance.trusted_pending).to_sat();
    let pending = (balance.untrusted_pending + balance.immature).to_sat();
    if json {
        println!(
            "{}",
            serde_json::json!({ "balance_sat": spendable, "pending_sat": pending, "synced": synced })
        );
    } else {
        let mut rows = vec![("Balance", spendable)];
        if pending > 0 {
            rows.push(("Pending", pending));
        }
        ui::sat_rows(&rows);
    }
    Ok(())
}
