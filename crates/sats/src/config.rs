use std::collections::BTreeMap;
use std::fs;

use anyhow::{Context, Result, bail};
use sats_core::bitcoin::Network;
use serde::{Deserialize, Serialize};

use crate::store::{Store, write_atomic};

#[derive(Serialize, Deserialize)]
pub struct Config {
    pub network: String,
    /// Human-selected confirmation target per network. Providers report fee
    /// estimates; this local policy chooses which estimate may be used.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fee_targets: BTreeMap<String, u16>,
    /// Legacy per-network esplora URLs. Still honored (as a chain provider
    /// below any `[providers.*]` entry), never written by new installs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub esplora: BTreeMap<String, String>,
    /// Typed providers: endpoints advertising capabilities. See
    /// `crate::provider` for resolution rules.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderConfig>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let endpoints: BTreeMap<_, _> = self
            .esplora
            .iter()
            .map(|(network, url)| (network, crate::provider::error::redact_url(url)))
            .collect();
        f.debug_struct("Config")
            .field("network", &self.network)
            .field("fee_targets", &self.fee_targets)
            .field("esplora", &endpoints)
            .field("providers", &self.providers)
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Driver kind: "esplora" | "subfrost".
    pub driver: String,
    /// The one network this endpoint serves.
    pub network: String,
    pub url: String,
    /// Restrict what this provider is used for. Tokens are capability names
    /// ("chain.sync", "guard.ord", ...) or the group aliases "chain" and
    /// "guard". Absent uses the driver's conservative default capabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    /// Subfrost API key, sent only as `x-subfrost-api-key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("driver", &self.driver)
            .field("network", &self.network)
            .field("url", &crate::provider::error::redact_url(&self.url))
            .field("capabilities", &self.capabilities)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("auth", &self.auth)
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Sent as `Authorization: Bearer <token>` on Esplora providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer: Option<String>,
}

impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("bearer", &self.bearer.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            network: "signet".into(),
            fee_targets: BTreeMap::new(),
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
        // TOML errors include source excerpts (and sometimes field values).
        // The source may be a URL or authentication field, so retain only its
        // location and never attach the original parser error as a source.
        let config: Config = toml::from_str(&text).map_err(|error: toml::de::Error| {
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

    pub fn save(&self, store: &Store) -> Result<()> {
        self.validate()?;
        let text = toml::to_string_pretty(self)?;
        write_atomic(&store.config_path(), text.as_bytes(), false)
    }

    fn validate(&self) -> Result<()> {
        for (network, target) in &self.fee_targets {
            parse_network(network)
                .with_context(|| format!("invalid fee target network {network:?}"))?;
            if !(1..=1008).contains(target) {
                bail!("{network} confirmation target must be between 1 and 1008 blocks");
            }
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_target_config_is_backward_compatible_and_validated() {
        let old: Config = toml::from_str("network = \"signet\"").unwrap();
        assert!(old.fee_targets.is_empty());
        assert!(old.validate().is_ok());

        let configured: Config =
            toml::from_str("network = \"signet\"\n[fee_targets]\nsignet = 1008\n").unwrap();
        assert_eq!(configured.fee_targets.get("signet"), Some(&1008));
        assert!(configured.validate().is_ok());

        for invalid in [
            "network = \"signet\"\n[fee_targets]\nsinget = 6\n",
            "network = \"signet\"\n[fee_targets]\nsignet = 0\n",
        ] {
            let config: Config = toml::from_str(invalid).unwrap();
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn authentication_debug_output_is_redacted() {
        let auth = AuthConfig {
            bearer: Some("bearer-secret".into()),
        };
        let debug = format!("{auth:?}");
        assert!(!debug.contains("bearer-secret"));
        assert_eq!(debug.matches("[REDACTED]").count(), 1);

        let provider = ProviderConfig {
            driver: "subfrost".into(),
            network: "signet".into(),
            url: "https://USERSECRET:PASSSECRET@signet.subfrost.io/PATHSECRET?unknown=QUERYSECRET#FRAGMENTSECRET".into(),
            capabilities: None,
            api_key: Some("subfrost-secret".into()),
            auth: None,
        };
        let debug = format!("{provider:?}");
        assert!(!debug.contains("subfrost-secret"));
        assert_eq!(debug.matches("[REDACTED]").count(), 1);
        let mut config = Config::default();
        config.esplora.insert("signet".into(), provider.url.clone());
        config.providers.insert("fixture".into(), provider);
        let cli =
            crate::provider::parse_cli_provider(&format!("esplora={}", config.esplora["signet"]))
                .unwrap();
        for debug in [format!("{config:#?}"), format!("{cli:#?}")] {
            for secret in [
                "USERSECRET",
                "PASSSECRET",
                "PATHSECRET",
                "QUERYSECRET",
                "FRAGMENTSECRET",
                "subfrost-secret",
            ] {
                assert!(!debug.contains(secret));
            }
        }
    }

    #[test]
    fn invalid_provider_config_never_echoes_secret_source_lines() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        for text in [
            "network = 'signet'\n[providers.fixture]\nurl = 'PATHSECRET\n",
            "network = 'signet'\n[providers.fixture]\ndriver = 'esplora'\nnetwork = 'signet'\nurl = 'http://host/private'\nauth = { bearer = ['BEARERSECRET'] }\n",
        ] {
            std::fs::write(store.config_path(), text).unwrap();
            let error = Config::load(&store).unwrap_err();
            for rendered in [format!("{error:#}"), format!("{error:#?}")] {
                assert!(rendered.contains("invalid config"));
                assert!(rendered.contains("line"));
                assert!(!rendered.contains("PATHSECRET"));
                assert!(!rendered.contains("BEARERSECRET"));
            }
        }
    }

    #[test]
    fn subfrost_api_key_is_a_direct_provider_field() {
        let direct = r#"
network = "signet"

[providers.subfrost]
driver = "subfrost"
network = "signet"
url = "https://signet.subfrost.io/v4/jsonrpc"
api_key = "secret"
"#;
        let config: Config = toml::from_str(direct).unwrap();
        assert_eq!(
            config.providers["subfrost"].api_key.as_deref(),
            Some("secret")
        );

        let nested = r#"
network = "signet"

[providers.subfrost]
driver = "subfrost"
network = "signet"
url = "https://signet.subfrost.io/v4/jsonrpc"

[providers.subfrost.auth]
api_key = "secret"
"#;
        assert!(toml::from_str::<Config>(nested).is_err());
    }
}
