//! The human CLI's default network. Grants and MCP launch commands keep
//! their explicit network; changing this setting never rewrites them.

use anyhow::Result;
use sats_core::bitcoin::Network;
use sats_wallet::config::{Config, network_name};
use sats_wallet::store::Store;

pub fn show(config: &Config, network: Network, json: bool) -> Result<()> {
    let active = network_name(network);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "network": active,
                "default_network": config.network,
            })
        );
    } else if active == config.network {
        println!("{active} (default)");
    } else {
        println!("{active} (override; default: {})", config.network);
    }
    Ok(())
}

pub fn use_network(store: &Store, mut config: Config, network: Network, json: bool) -> Result<()> {
    config.network = network_name(network).to_string();
    config.save(store)?;
    show(&config, network, json)
}
