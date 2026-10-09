use anyhow::Result;
use bdk_wallet::KeychainKind;
use sats_core::bitcoin::Network;
use sats_wallet::store::Store;
use sats_wallet::walletd;

use crate::ui;

pub fn run(store: &Store, network: Network, json: bool) -> Result<()> {
    let mut ctx = walletd::open(store, network)?;
    let info = ctx.wallet.reveal_next_address(KeychainKind::External);
    ctx.persist()?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "address": info.address.to_string(), "index": info.index })
        );
    } else {
        println!("{}", info.address);
        ui::dim(&format!("index {}", info.index));
    }
    Ok(())
}
