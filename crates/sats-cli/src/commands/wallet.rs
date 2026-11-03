//! `sats wallet`: named wallets, one seed each. The wallet and the
//! network are independent choices; grants, requests, and agent
//! connections belong to one wallet on one network and stay pinned there.

use anyhow::{Result, bail};
use sats_core::bitcoin::Network;
use sats_wallet::config::{Config, network_name};
use sats_wallet::provider::CliProvider;
use sats_wallet::wallets::Wallets;

use crate::commands::init;
use crate::ui;

pub fn show(
    wallets: &Wallets,
    config: &Config,
    wallet: &str,
    network: Network,
    json: bool,
) -> Result<()> {
    let exists = wallets.exists(wallet);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "wallet": wallet,
                "exists": exists,
                "default_wallet": config.wallet,
                "network": network_name(network),
            })
        );
        return Ok(());
    }
    ui::kv_rows(&[
        ("Wallet", wallet.to_string()),
        ("Network", network_name(network).to_string()),
    ]);
    if !exists {
        println!();
        println!("No wallet named {wallet:?} yet: sats wallet create {wallet}");
    }
    Ok(())
}

pub fn list(wallets: &Wallets, config: &Config, selected: &str, json: bool) -> Result<()> {
    let entries = wallets.list()?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "selected": selected,
                "default_wallet": config.wallet,
                "wallets": entries.iter().map(|w| serde_json::json!({
                    "name": w.name,
                    "networks": w.networks,
                })).collect::<Vec<_>>(),
            })
        );
        return Ok(());
    }
    if entries.is_empty() {
        println!("No wallets yet: sats wallet create <name>, or sats wallet import <name>");
        return Ok(());
    }
    for entry in entries {
        let marker = if entry.name == selected { "*" } else { " " };
        let networks = if entry.networks.is_empty() {
            "no networks yet".to_string()
        } else {
            entry.networks.join(", ")
        };
        println!("{marker} {:<16} {networks}", entry.name);
    }
    Ok(())
}

pub fn create(
    wallets: &Wallets,
    config: Config,
    network: Network,
    name: &str,
    words: u8,
    overrides: Option<&CliProvider>,
) -> Result<()> {
    let store = wallets.store(name)?;
    if wallets.exists(name) {
        bail!("a wallet named {name:?} already exists — pick another name");
    }
    wallets.harden()?;
    let first = wallets.list()?.is_empty();
    init::run(&store, config, network, Some(words), false, overrides)?;
    hint_switch(wallets, name, first)
}

pub fn import(
    wallets: &Wallets,
    config: Config,
    network: Network,
    name: &str,
    overrides: Option<&CliProvider>,
) -> Result<()> {
    let store = wallets.store(name)?;
    if wallets.exists(name) {
        bail!("a wallet named {name:?} already exists — pick another name");
    }
    wallets.harden()?;
    let first = wallets.list()?.is_empty();
    init::run(&store, config, network, None, true, overrides)?;
    hint_switch(wallets, name, first)
}

/// A first wallet becomes the default; later ones are opt-in.
fn hint_switch(wallets: &Wallets, name: &str, first: bool) -> Result<()> {
    let store = wallets.config_store();
    let mut config = Config::load(&store)?;
    if first && config.wallet.is_none() {
        config.wallet = Some(name.to_string());
        config.save(&store)?;
    } else if config.wallet.as_deref() != Some(name) {
        println!("Switch to it: sats wallet use {name}   (or --wallet {name} for one command)");
    }
    Ok(())
}

pub fn use_wallet(wallets: &Wallets, mut config: Config, name: &str, json: bool) -> Result<()> {
    wallets.store(name)?;
    if !wallets.exists(name) {
        bail!("no wallet named {name:?} — see sats wallet list");
    }
    config.wallet = Some(name.to_string());
    config.save(&wallets.config_store())?;
    if json {
        println!("{}", serde_json::json!({"default_wallet": name}));
    } else {
        ui::ok(&format!("default wallet  {name}"));
    }
    Ok(())
}
