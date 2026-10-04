use std::fs;

use anyhow::{Context, Result, bail};
use sats_core::bitcoin::Network;
use serde::{Deserialize, Serialize};

use crate::provider::error::redact_url;
use crate::store::{Store, write_atomic};

/// The human's settings. Each network's provider choice — where its chain
/// data comes from — lives under `[<network>]`. The Subfrost key is shared
/// by every network. See `crate::provider` for how these become services.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub network: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subfrost: Option<SubfrostConfig>,
    #[serde(default, skip_serializing_if = "NetworkConfig::is_empty")]
    pub mainnet: NetworkConfig,
    #[serde(default, skip_serializing_if = "NetworkConfig::is_empty")]
    pub signet: NetworkConfig,
    #[serde(default, skip_serializing_if = "NetworkConfig::is_empty")]
    pub testnet4: NetworkConfig,
    #[serde(default, skip_serializing_if = "NetworkConfig::is_empty")]
    pub regtest: NetworkConfig,
}

/// Where a network's Bitcoin data (sync, fee estimates, broadcast) comes
/// from. Exactly one source per network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChainChoice {
    /// The public mempool.space Esplora API.
    Mempool,
    /// Subfrost, with the saved key.
    Subfrost,
    /// The Esplora server configured under `[<network>.esplora]`.
    Esplora,
}

impl ChainChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            ChainChoice::Mempool => "mempool",
            ChainChoice::Subfrost => "subfrost",
            ChainChoice::Esplora => "esplora",
        }
    }

    /// The source a network uses when nothing is chosen. mempool.space
    /// serves no regtest, so regtest defaults to a local Esplora.
    pub fn default_for(network: Network) -> ChainChoice {
        match network {
            Network::Regtest => ChainChoice::Esplora,
            _ => ChainChoice::Mempool,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    /// Absent: [`ChainChoice::default_for`] the network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<ChainChoice>,
    /// Human-selected confirmation target in blocks (1–1008, default 2).
    /// Providers report fee estimates; this local policy chooses which
    /// estimate may be used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_target: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub esplora: Option<EsploraConfig>,
    /// Subfrost endpoint for this network, replacing the built-in one
    /// (required where Subfrost publishes none, such as regtest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subfrost_url: Option<RedactedUrl>,
}

impl NetworkConfig {
    pub fn is_empty(&self) -> bool {
        *self == NetworkConfig::default()
    }
}

/// An endpoint URL that may embed credentials: `Debug` shows only its
/// origin.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RedactedUrl(pub String);

impl std::fmt::Debug for RedactedUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&redact_url(&self.0))
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubfrostConfig {
    /// Sent only as `x-subfrost-api-key`.
    pub api_key: String,
}

impl std::fmt::Debug for SubfrostConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubfrostConfig")
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EsploraConfig {
    pub url: RedactedUrl,
    /// Sent as `Authorization: Bearer <token>` on reads and broadcast.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer: Option<String>,
}

impl std::fmt::Debug for EsploraConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EsploraConfig")
            .field("url", &self.url)
            .field("bearer", &self.bearer.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            network: "signet".into(),
            subfrost: None,
            mainnet: NetworkConfig::default(),
            signet: NetworkConfig::default(),
            testnet4: NetworkConfig::default(),
            regtest: NetworkConfig::default(),
        }
    }
}

/// Sections of the pre-release provider format, refused with guidance.
const UNSUPPORTED_SECTIONS: [&str; 3] = ["providers", "esplora", "fee_targets"];

impl Config {
    pub fn load(store: &Store) -> Result<Config> {
        let path = store.config_path();
        if !path.exists() {
            return Ok(Config::default());
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        // TOML errors include source excerpts (and sometimes field values).
        // The source may be a URL or authentication field, so retain only its
        // location and never attach the original parser error as a source.
        let config: Config = toml::from_str(&text).map_err(|error: toml::de::Error| {
            if let Ok(table) = toml::from_str::<toml::Table>(&text)
                && UNSUPPORTED_SECTIONS
                    .iter()
                    .any(|key| table.contains_key(*key))
            {
                return anyhow::anyhow!(
                    "config {} uses the unsupported [providers]/[esplora]/[fee_targets] format — \
                     remove those sections, then set providers up again with `sats providers add`; \
                     a fee target is now `fee_target` under [<network>]",
                    path.display()
                );
            }
            let location = error
                .span()
                .map(|span| {
                    let line = text.as_bytes()[..span.start.min(text.len())]
                        .iter()
                        .filter(|&&byte| byte == b'\n')
                        .count()
                        + 1;
                    format!(" at line {line}")
                })
                .unwrap_or_default();
            anyhow::anyhow!(
                "invalid config {}{location}: invalid TOML or configuration fields",
                path.display()
            )
        })?;
        config
            .validate()
            .with_context(|| format!("invalid config {}", path.display()))?;
        Ok(config)
    }

    /// Owner-only: the Subfrost key and Esplora bearer tokens live here.
    pub fn save(&self, store: &Store) -> Result<()> {
        self.validate()?;
        let text = toml::to_string_pretty(self)?;
        write_atomic(&store.config_path(), text.as_bytes(), true)
    }

    /// Shape checks only; provider choices that cannot be served (no
    /// Esplora URL, Subfrost not set up) are resolution errors, so
    /// local commands keep working while they are fixed.
    fn validate(&self) -> Result<()> {
        if self
            .subfrost
            .as_ref()
            .is_some_and(|s| s.api_key.trim().is_empty())
        {
            bail!("the Subfrost api_key must not be empty");
        }
        for (name, net) in self.networks() {
            if let Some(target) = net.fee_target
                && !(1..=1008).contains(&target)
            {
                bail!("{name} fee_target must be between 1 and 1008 blocks");
            }
            if let Some(esplora) = &net.esplora {
                if esplora.url.0.trim().is_empty() {
                    bail!("{name} Esplora url must not be empty");
                }
                if esplora.bearer.as_ref().is_some_and(|b| b.trim().is_empty()) {
                    bail!("{name} Esplora bearer token must not be empty");
                }
            }
            if net
                .subfrost_url
                .as_ref()
                .is_some_and(|u| u.0.trim().is_empty())
            {
                bail!("{name} subfrost_url must not be empty");
            }
        }
        Ok(())
    }

    pub fn networks(&self) -> [(&'static str, &NetworkConfig); 4] {
        [
            ("mainnet", &self.mainnet),
            ("signet", &self.signet),
            ("testnet4", &self.testnet4),
            ("regtest", &self.regtest),
        ]
    }

    /// One network's settings. Legacy testnet3 never parses, so it has
    /// none.
    pub fn net(&self, network: Network) -> &NetworkConfig {
        const NONE: &NetworkConfig = &NetworkConfig {
            chain: None,
            fee_target: None,
            esplora: None,
            subfrost_url: None,
        };
        match network {
            Network::Bitcoin => &self.mainnet,
            Network::Signet => &self.signet,
            Network::Testnet4 => &self.testnet4,
            Network::Regtest => &self.regtest,
            _ => NONE,
        }
    }

    pub fn net_mut(&mut self, network: Network) -> &mut NetworkConfig {
        match network {
            Network::Bitcoin => &mut self.mainnet,
            Network::Signet => &mut self.signet,
            Network::Testnet4 => &mut self.testnet4,
            Network::Regtest => &mut self.regtest,
            _ => unreachable!("legacy testnet3 is refused by parse_network"),
        }
    }

    /// The public mempool.space Esplora API for a network.
    pub fn mempool_url(network: Network) -> Option<&'static str> {
        match network {
            Network::Bitcoin => Some("https://mempool.space/api"),
            Network::Signet => Some("https://mempool.space/signet/api"),
            Network::Testnet4 => Some("https://mempool.space/testnet4/api"),
            _ => None,
        }
    }

    /// Where `chain = "esplora"` points without a configured URL: a local
    /// regtest Esplora, and nowhere on public networks.
    pub fn default_esplora_url(network: Network) -> Option<&'static str> {
        match network {
            Network::Regtest => Some("http://localhost:3002"),
            _ => None,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(text: &str) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        std::fs::write(store.config_path(), text).unwrap();
        (dir, store)
    }

    #[test]
    fn provider_choices_round_trip_per_network() {
        let text = r#"
network = "signet"

[subfrost]
api_key = "secret"

[signet]
chain = "subfrost"
fee_target = 1008

[mainnet.esplora]
url = "https://bitcoin.example/api"
bearer = "token"
"#;
        let (_dir, store) = store_with(text);
        let config = Config::load(&store).unwrap();
        assert_eq!(config.signet.chain, Some(ChainChoice::Subfrost));
        assert_eq!(config.signet.fee_target, Some(1008));
        assert_eq!(config.subfrost.as_ref().unwrap().api_key, "secret");
        assert!(config.mainnet.chain.is_none());
        assert!(config.testnet4.is_empty());

        config.save(&store).unwrap();
        let again = Config::load(&store).unwrap();
        assert_eq!(again.signet, config.signet);
        assert_eq!(again.mainnet, config.mainnet);
        let saved = std::fs::read_to_string(store.config_path()).unwrap();
        assert!(!saved.contains("[testnet4]"), "empty networks are omitted");
    }

    #[test]
    fn shape_errors_are_refused_at_load() {
        for invalid in [
            "network = \"signet\"\n[signet]\nfee_target = 0\n",
            "network = \"signet\"\n[signet]\nfee_target = 1009\n",
            "network = \"signet\"\n[signet]\nchain = \"electrum\"\n",
            "network = \"signet\"\n[signte]\nchain = \"mempool\"\n",
            "network = \"signet\"\n[signet]\nprotect_assets = true\n",
            "network = \"signet\"\n[subfrost]\napi_key = \" \"\n",
        ] {
            let (_dir, store) = store_with(invalid);
            let error = Config::load(&store).unwrap_err();
            assert!(format!("{error:#}").contains("invalid config"), "{invalid}");
        }
    }

    #[test]
    fn the_previous_provider_format_is_refused_with_guidance() {
        for old in [
            "network = 'signet'\n[providers.subfrost]\ndriver = 'subfrost'\nnetwork = 'signet'\nurl = 'https://PATHSECRET'\napi_key = 'KEYSECRET'\n",
            "network = 'signet'\n[esplora]\nsignet = 'https://PATHSECRET'\n",
            "network = 'signet'\n[fee_targets]\nsignet = 6\n",
        ] {
            let (_dir, store) = store_with(old);
            let rendered = format!("{:#}", Config::load(&store).unwrap_err());
            assert!(rendered.contains("unsupported"), "{rendered}");
            assert!(rendered.contains("sats providers add"), "{rendered}");
            assert!(!rendered.contains("PATHSECRET") && !rendered.contains("KEYSECRET"));
        }
    }

    #[test]
    fn credentials_are_redacted_from_debug_output() {
        let mut config = Config {
            subfrost: Some(SubfrostConfig {
                api_key: "subfrost-secret".into(),
            }),
            ..Config::default()
        };
        config.signet.esplora = Some(EsploraConfig {
            url: RedactedUrl(
                "https://USERSECRET:PASSSECRET@host.example/PATHSECRET?q=QUERYSECRET#FRAGMENTSECRET"
                    .into(),
            ),
            bearer: Some("bearer-secret".into()),
        });
        config.mainnet.subfrost_url = Some(RedactedUrl("https://host.example/KEYPATH".into()));
        for debug in [format!("{config:?}"), format!("{config:#?}")] {
            assert!(debug.contains("https://host.example"));
            for secret in [
                "subfrost-secret",
                "bearer-secret",
                "USERSECRET",
                "PASSSECRET",
                "PATHSECRET",
                "QUERYSECRET",
                "FRAGMENTSECRET",
                "KEYPATH",
            ] {
                assert!(!debug.contains(secret), "{debug}");
            }
        }
    }

    #[test]
    fn invalid_config_never_echoes_secret_source_lines() {
        for text in [
            "network = 'signet'\n[signet.esplora]\nurl = 'PATHSECRET\n",
            "network = 'signet'\n[signet.esplora]\nurl = 'http://host/private'\nbearer = ['BEARERSECRET']\n",
            "network = 'signet'\n[subfrost]\napi_key = 'KEYSECRET'\nextra = 'EXTRASECRET'\n",
        ] {
            let (_dir, store) = store_with(text);
            let error = Config::load(&store).unwrap_err();
            for rendered in [format!("{error:#}"), format!("{error:#?}")] {
                assert!(rendered.contains("invalid config"));
                assert!(rendered.contains("line"));
                for secret in ["PATHSECRET", "BEARERSECRET", "KEYSECRET", "EXTRASECRET"] {
                    assert!(!rendered.contains(secret), "{rendered}");
                }
            }
        }
    }

    #[test]
    fn saved_config_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        let config = Config {
            subfrost: Some(SubfrostConfig {
                api_key: "secret".into(),
            }),
            ..Config::default()
        };
        config.save(&store).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.config_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
