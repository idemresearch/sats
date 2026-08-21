use std::collections::BTreeMap;
use std::fs;

use anyhow::{Context, Result, bail};
use sats_core::bitcoin::Network;
use serde::{Deserialize, Serialize};

use crate::store::{Store, write_atomic};

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub network: String,
    /// Legacy per-network esplora URLs. Still honored (as a chain provider
    /// below any `[providers.*]` entry), never written by new installs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub esplora: BTreeMap<String, String>,
    /// Typed providers: endpoints advertising capabilities. See
    /// `crate::provider` for resolution rules.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Driver kind: "esplora" | "subfrost".
    pub driver: String,
    /// The one network this endpoint serves.
    pub network: String,
    pub url: String,
    /// Restrict what this provider is used for. Tokens are capability names
    /// ("chain.sync", "guard.ord", ...) or the group aliases "chain" and
    /// "guard". Absent = everything the driver offers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Sent as `Authorization: Bearer <token>` on REST providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            network: "signet".into(),
            esplora: BTreeMap::new(),
            providers: BTreeMap::new(),
        }
    }
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

    /// Built-in esplora fallback for a network, used only when neither
    /// `[providers.*]` nor the legacy `[esplora]` map covers it.
    pub fn builtin_esplora_url(network: &str) -> Option<String> {
        Some(
            match network {
                "mainnet" => "https://mempool.space/api",
                "signet" => "https://mempool.space/signet/api",
                "testnet4" => "https://mempool.space/testnet4/api",
                "regtest" => "http://localhost:3002",
                _ => return None,
            }
            .to_string(),
        )
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
