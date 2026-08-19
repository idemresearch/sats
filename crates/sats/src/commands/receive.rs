use anyhow::Result;
use bdk_wallet::KeychainKind;
use sats_core::bitcoin::Network;

use crate::config::Config;
use crate::store::Store;
use crate::{ui, walletd};

pub fn run(store: &Store, config: &Config, network: Network, json: bool) -> Result<()> {
    let mut ctx = walletd::open(store, config, network)?;
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
