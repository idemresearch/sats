use std::collections::BTreeMap;
use std::fs;

use anyhow::{Context, Result, bail};
use sats_core::bitcoin::Network;
use serde::{Deserialize, Serialize};

use crate::store::{Store, write_atomic};

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub network: String,
    #[serde(default = "default_esplora")]
    pub esplora: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            network: "signet".into(),
            esplora: default_esplora(),
        }
    }
}

fn default_esplora() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("mainnet".into(), "https://mempool.space/api".into()),
        ("signet".into(), "https://mempool.space/signet/api".into()),
        (
            "testnet4".into(),
            "https://mempool.space/testnet4/api".into(),
        ),
        ("regtest".into(), "http://localhost:3002".into()),
    ])
}

impl Config {
    pub fn load(store: &Store) -> Result<Config> {
        let path = store.config_path();
        if !path.exists() {
            return Ok(Config::default());
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("invalid config {}", path.display()))
    }

    pub fn save(&self, store: &Store) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        write_atomic(&store.config_path(), text.as_bytes(), false)
    }

    pub fn esplora_url(&self, network: &str) -> Result<String> {
        match self.esplora.get(network) {
            Some(url) => Ok(url.clone()),
            None => default_esplora()
                .get(network)
                .cloned()
                .with_context(|| format!("no esplora url configured for {network}")),
        }
    }
}

/// Canonical network name used for directories, config, and grant records.
pub fn network_name(network: Network) -> &'static str {
    match network {
        Network::Bitcoin => "mainnet",
        Network::Signet => "signet",
        Network::Testnet4 => "testnet4",
        Network::Regtest => "regtest",
        _ => "testnet",
    }
}

pub fn parse_network(name: &str) -> Result<Network> {
    Ok(match name {
        "mainnet" | "bitcoin" => Network::Bitcoin,
        "signet" => Network::Signet,
        "testnet4" => Network::Testnet4,
        "regtest" => Network::Regtest,
        "testnet" => bail!("legacy testnet3 is not supported — use testnet4"),
        other => bail!("unknown network {other:?} (use mainnet, signet, testnet4, or regtest)"),
    })
}
