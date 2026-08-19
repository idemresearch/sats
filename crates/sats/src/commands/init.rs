use anyhow::{Context, Result, bail};
use sats_core::bitcoin::Network;
use sats_core::{seal, seed};

use crate::config::{Config, network_name};
use crate::store::{AAD_SEED, Store};
use crate::{password, ui, walletd};

pub fn run(store: &Store, mut config: Config, network: Network, words: u8) -> Result<()> {
    let net_name = network_name(network);

    if store.seed_exists() {
        // The seed is network-independent: extend it to a new network
        // instead of refusing outright.
        if store.wallet_db_path(net_name).exists() {
            bail!(
                "wallet already exists ({}) — to start over, delete your sats data directory manually",
                store.seed_path().display()
            );
        }
        let pw = password::get(false)?;
        let blob = store.read_seed()?;
        let mnemonic_bytes = seal::open(&blob, pw.as_bytes(), AAD_SEED)?;
        let mnemonic =
            seed::parse_mnemonic(std::str::from_utf8(&mnemonic_bytes).context("corrupt seed")?)?;
        let (ext, int) = seed::public_descriptors(&mnemonic, network)?;
        walletd::create(store, network, ext, int)?;
        ui::ok(&format!("wallet extended to {net_name}"));
        return Ok(());
    }

    let mnemonic = seed::generate_mnemonic(words)?;
    let pw = password::get(true)?;
    let blob = seal::seal(mnemonic.to_string().as_bytes(), pw.as_bytes(), AAD_SEED)?;
    store.write_seed(&blob)?;

    let (ext, int) = seed::public_descriptors(&mnemonic, network)?;
    walletd::create(store, network, ext, int)?;

    config.network = net_name.to_string();
    config.save(store)?;

    println!();
    let phrase = mnemonic.to_string();
    let word_list: Vec<&str> = phrase.split(' ').collect();
    for chunk in word_list.chunks(6) {
        println!("  {}", chunk.join(" "));
    }
    println!();
    ui::warn(&format!(
        "write these {words} words down — they will not be shown again"
    ));
    println!();
    ui::ok(&format!("wallet created  {net_name}"));
    Ok(())
}
