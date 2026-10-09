//! Chain providers.
//!
//! Each network has exactly one chain source, chosen by `[<network>]
//! chain`: the public mempool.space Esplora (the default), Subfrost, or a
//! configured Esplora server. It serves sync, fee estimates, broadcast,
//! and, in experimental builds, Alkanes views, which only Subfrost serves.
//! Drivers are audited enums, not a plugin surface, and `sats-core` never
//! sees any of this.
//!
//! Resolution is pure config work (no network I/O). A `--provider`
//! override replaces the chain source for one invocation.

pub mod error;
pub mod esplora;
pub mod mock;
pub mod subfrost;

use std::collections::HashMap;

use sats_core::bitcoin::{FeeRate, Network, Transaction, Txid};

use crate::config::{ChainChoice, Config, network_name};
use crate::walletd::WalletCtx;

pub use error::ProviderError;

use esplora::EsploraProvider;
use mock::MockProvider;
use subfrost::SubfrostClient;

/// Per HTTP request, not a deadline for an entire multi-request wallet scan.
pub const HTTP_TIMEOUT_SECS: u64 = 30;

/// The driver a `--provider KIND=URL` override names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverKind {
    Esplora,
    Subfrost,
}

/// One CLI `--provider KIND=URL` override.
#[derive(Clone)]
pub struct CliProvider {
    pub kind: DriverKind,
    pub url: String,
}

impl std::fmt::Debug for CliProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliProvider")
            .field("kind", &self.kind)
            .field("url", &error::redact_url(&self.url))
            .finish()
    }
}

/// clap value parser for `--provider KIND=URL`. Splits on the first `=`
/// only, so URLs carrying `=` (API keys) survive intact.
pub fn parse_cli_provider(s: &str) -> Result<CliProvider, String> {
    let (kind, url) = s
        .split_once('=')
        .ok_or_else(|| "expected KIND=URL (e.g. esplora=https://mempool.space/api)".to_string())?;
    let kind = match kind {
        "esplora" => DriverKind::Esplora,
        "subfrost" => DriverKind::Subfrost,
        _ => return Err("unknown provider kind (use esplora or subfrost)".to_string()),
    };
    if url.is_empty() {
        return Err("expected KIND=URL with a non-empty url".to_string());
    }
    Ok(CliProvider {
        kind,
        url: url.to_string(),
    })
}

/// Where the endpoint in use came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Built in: nothing was chosen.
    Default,
    /// The human's config.
    Config,
    /// A `--provider` value for this invocation.
    Override,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::Config => "config",
            Source::Override => "override",
        }
    }
}

/// Which credential, if any, an endpoint sends. Never the credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    None,
    ApiKey,
    Bearer,
}

impl AuthKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthKind::None => "none",
            AuthKind::ApiKey => "api_key",
            AuthKind::Bearer => "bearer",
        }
    }
}

/// One endpoint in use, display-safe: the redacted origin, never the
/// configured URL or its credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// `mempool`, `esplora`, or `subfrost`.
    pub provider: &'static str,
    pub url: String,
    pub auth: AuthKind,
    pub source: Source,
}

/// A chain-data transport. One source fills sync, fees, and broadcast.
#[derive(Debug, Clone)]
pub enum ChainSource {
    Esplora(EsploraProvider),
    Subfrost(SubfrostClient),
    Mock(MockProvider),
}

/// The resolved service set for one network.
#[derive(Debug)]
pub struct Services {
    network: Network,
    fee_target_blocks: u16,
    sync: ChainSource,
    fees: ChainSource,
    broadcast: ChainSource,
}

/// The resolved choices before they become services.
struct Plan {
    fee_target_blocks: u16,
    chain: (ChainSource, Endpoint),
}

/// Resolve the services for `network` from config and an optional CLI
/// override. Pure: no network I/O. Call when a workflow reaches
/// chain-dependent work.
pub fn resolve(
    config: &Config,
    cli: Option<&CliProvider>,
    network: Network,
) -> Result<Services, ProviderError> {
    let plan = plan(config, cli, network)?;
    let sync = plan.chain.0;
    // Test seam, mock only: hand one role to a real Esplora endpoint so
    // integration tests reach the HTTP transport behind a deterministic
    // sync. Production sources always serve all three roles.
    let delegated = |role: &str| match &sync {
        ChainSource::Mock(m) => m
            .delegate(role)
            .map(|url| ChainSource::Esplora(EsploraProvider::new(role.into(), url, None))),
        _ => None,
    };
    let fees = delegated("fees-via").unwrap_or_else(|| sync.clone());
    let broadcast = delegated("broadcast-via").unwrap_or_else(|| sync.clone());
    Ok(Services {
        network,
        fee_target_blocks: plan.fee_target_blocks,
        sync,
        fees,
        broadcast,
    })
}

/// Describe the provider [`resolve`] would use for `network`. Pure, and
/// fails exactly when `resolve` fails.
pub fn overview(
    config: &Config,
    cli: Option<&CliProvider>,
    network: Network,
) -> Result<Endpoint, ProviderError> {
    Ok(plan(config, cli, network)?.chain.1)
}

fn plan(
    config: &Config,
    cli: Option<&CliProvider>,
    network: Network,
) -> Result<Plan, ProviderError> {
    let net_name = network_name(network);
    let net = config.net(network);
    let setup = |reason: String| ProviderError::Setup {
        network: net_name,
        reason,
    };
    let fee_target_blocks = net.fee_target.unwrap_or(2);
    if !(1..=1008).contains(&fee_target_blocks) {
        return Err(setup("fee_target must be between 1 and 1008 blocks".into()));
    }

    let chain = match cli {
        Some(p) => match p.kind {
            DriverKind::Esplora => esplora_chain("cli:esplora", &p.url, None, Source::Override),
            // The saved key never follows an override: it would go to
            // whatever URL was typed.
            DriverKind::Subfrost => subfrost_chain(&p.url, None, Source::Override),
        }
        .map_err(setup)?,
        None => configured_chain(config, network).map_err(setup)?,
    };
    Ok(Plan {
        fee_target_blocks,
        chain,
    })
}

fn configured_chain(config: &Config, network: Network) -> Result<(ChainSource, Endpoint), String> {
    let net = config.net(network);
    let chosen = net.chain.is_some();
    match net.chain.unwrap_or(ChainChoice::default_for(network)) {
        ChainChoice::Mempool => {
            let url = Config::mempool_url(network).ok_or(
                "mempool.space doesn't serve this network — run \
                 `sats providers add esplora --url URL`",
            )?;
            let source = if chosen {
                Source::Config
            } else {
                Source::Default
            };
            esplora_chain("mempool", url, None, source)
        }
        ChainChoice::Esplora => match (&net.esplora, Config::default_esplora_url(network)) {
            (Some(esplora), _) => esplora_chain(
                "esplora",
                &esplora.url.0,
                esplora.bearer.clone(),
                Source::Config,
            ),
            (None, Some(url)) => esplora_chain("esplora", url, None, Source::Default),
            (None, None) => Err("the provider is set to esplora, but no Esplora URL is \
                 configured — run `sats providers add esplora --url URL`"
                .into()),
        },
        ChainChoice::Subfrost => subfrost_endpoint(config, network)?.ok_or_else(|| {
            "the provider is set to subfrost, but Subfrost isn't set up for \
             this network — run `sats providers add subfrost`"
                .into()
        }),
    }
}

/// The Subfrost endpoint for a network, when Subfrost is set up there: a
/// saved key (with the built-in endpoint, where one exists) or an
/// explicit `subfrost_url`.
fn subfrost_endpoint(
    config: &Config,
    network: Network,
) -> Result<Option<(ChainSource, Endpoint)>, String> {
    let api_key = config.subfrost.as_ref().map(|s| s.api_key.clone());
    let url = match &config.net(network).subfrost_url {
        Some(url) => url.0.clone(),
        None if api_key.is_some() => match subfrost::default_url(network) {
            Some(url) => url.to_string(),
            None => return Ok(None),
        },
        None => return Ok(None),
    };
    subfrost_chain(&url, api_key, Source::Config).map(Some)
}

/// The undocumented file-driven test driver answers any `file://`
/// endpoint; see [`mock`].
fn mock_chain(url: &str, source: Source) -> Option<Result<(ChainSource, Endpoint), String>> {
    url.starts_with("file://").then(|| {
        let mock = MockProvider::new(url)?;
        let endpoint = Endpoint {
            provider: "mock",
            url: mock.display_url().to_string(),
            auth: AuthKind::None,
            source,
        };
        Ok((ChainSource::Mock(mock), endpoint))
    })
}

fn esplora_chain(
    name: &'static str,
    url: &str,
    bearer: Option<String>,
    source: Source,
) -> Result<(ChainSource, Endpoint), String> {
    if let Some(mock) = mock_chain(url, source) {
        return mock;
    }
    let auth = if bearer.is_some() {
        AuthKind::Bearer
    } else {
        AuthKind::None
    };
    let client = EsploraProvider::new(name.to_string(), url.to_string(), bearer);
    let endpoint = Endpoint {
        provider: if name == "mempool" {
            "mempool"
        } else {
            "esplora"
        },
        url: client.display_url().to_string(),
        auth,
        source,
    };
    Ok((ChainSource::Esplora(client), endpoint))
}

fn subfrost_chain(
    url: &str,
    api_key: Option<String>,
    source: Source,
) -> Result<(ChainSource, Endpoint), String> {
    if let Some(mock) = mock_chain(url, source) {
        return mock;
    }
    let auth = if api_key.is_some() {
        AuthKind::ApiKey
    } else {
        AuthKind::None
    };
    let client = SubfrostClient::new(url.to_string(), api_key);
    let endpoint = Endpoint {
        provider: "subfrost",
        url: client.display_url().to_string(),
        auth,
        source,
    };
    Ok((ChainSource::Subfrost(client), endpoint))
}

impl Services {
    /// Confirm the chain source answers and serves this network, before a
    /// human relies on it.
    pub fn check_chain(&self) -> Result<(), ProviderError> {
        match &self.sync {
            ChainSource::Esplora(e) => e.check_network(self.network),
            ChainSource::Subfrost(c) => c.check_network(self.network),
            ChainSource::Mock(m) => m.sync(),
        }
    }

    /// Sync the wallet: a full scan on first touch, incremental after.
    /// Validates the provider serves the wallet's network first.
    pub fn sync_wallet(&self, ctx: &mut WalletCtx) -> Result<(), ProviderError> {
        let status = crate::ui::StatusLine::start("syncing…");
        let result = self.sync_inner(ctx);
        status.finish();
        result
    }

    fn sync_inner(&self, ctx: &mut WalletCtx) -> Result<(), ProviderError> {
        let sync_err = |url: &str, message: String| ProviderError::Sync {
            url: url.to_string(),
            message,
        };
        match &self.sync {
            ChainSource::Esplora(e) => {
                e.check_network(ctx.network)?;
                if ctx.wallet.latest_checkpoint().height() == 0 {
                    let update = e.full_scan(ctx.wallet.start_full_scan())?;
                    ctx.wallet
                        .apply_update(update)
                        .map_err(|err| sync_err(e.display_url(), err.to_string()))?;
                } else {
                    let update = e.sync(ctx.wallet.start_sync_with_revealed_spks())?;
                    ctx.wallet
                        .apply_update(update)
                        .map_err(|err| sync_err(e.display_url(), err.to_string()))?;
                }
                ctx.persist()
                    .map_err(|err| sync_err(e.display_url(), format!("{err:#}")))
            }
            ChainSource::Subfrost(c) => {
                c.check_network(ctx.network)?;
                if ctx.wallet.latest_checkpoint().height() == 0 {
                    let update = c.full_scan(ctx.wallet.start_full_scan())?;
                    ctx.wallet
                        .apply_update(update)
                        .map_err(|err| sync_err(c.display_url(), err.to_string()))?;
                } else {
                    let update = c.sync(ctx.wallet.start_sync_with_revealed_spks())?;
                    ctx.wallet
                        .apply_update(update)
                        .map_err(|err| sync_err(c.display_url(), err.to_string()))?;
                }
                ctx.persist()
                    .map_err(|err| sync_err(c.display_url(), format!("{err:#}")))
            }
            ChainSource::Mock(m) => m.sync(),
        }
    }

    /// Estimated fee rate for the configured confirmation target, floored at
    /// 1 sat/vB. An unusable rate from the endpoint is a typed fee error,
    /// never a silently cast number.
    pub fn estimate_fee_rate(&self) -> Result<FeeRate, ProviderError> {
        let (estimates, url) = match &self.fees {
            ChainSource::Esplora(e) => (e.fee_estimates()?, e.display_url().to_string()),
            ChainSource::Subfrost(c) => (c.fee_estimates()?, c.display_url().to_string()),
            ChainSource::Mock(m) => (m.fee_estimates()?, "mock".to_string()),
        };
        pick_fee_rate(&estimates, self.fee_target_blocks)
            .map_err(|message| ProviderError::Fees { url, message })
    }

    /// Broadcast and record the transaction as unconfirmed in the wallet.
    pub fn broadcast(&self, ctx: &mut WalletCtx, tx: &Transaction) -> Result<Txid, ProviderError> {
        let txid = match &self.broadcast {
            ChainSource::Esplora(e) => {
                e.broadcast(tx)?;
                tx.compute_txid()
            }
            ChainSource::Subfrost(c) => c.broadcast(tx)?,
            ChainSource::Mock(m) => m.broadcast(tx)?,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        ctx.wallet.apply_unconfirmed_txs([(tx.clone(), now)]);
        let _ = ctx.persist();
        Ok(txid)
    }

    /// Alkanes views come from the network's one provider; only Subfrost
    /// serves them.
    #[cfg(feature = "experimental-alkanes")]
    fn no_alkanes(&self) -> ProviderError {
        ProviderError::NoAlkanes {
            network: network_name(self.network),
        }
    }

    /// Contract bytecode for an alkane id. Validates the endpoint's
    /// network before trusting its answer.
    #[cfg(feature = "experimental-alkanes")]
    pub fn alkanes_bytecode(&self, block: u128, tx: u128) -> Result<Vec<u8>, ProviderError> {
        match &self.sync {
            ChainSource::Subfrost(c) => {
                c.check_network(self.network)?;
                c.alkanes_bytecode(block, tx)
            }
            ChainSource::Mock(m) => m.alkanes_bytecode(block, tx),
            ChainSource::Esplora(_) => Err(self.no_alkanes()),
        }
    }

    /// Simulate a contract call; the result is the endpoint's verbatim
    /// JSON. Advisory only — a simulation never authorizes anything.
    #[cfg(feature = "experimental-alkanes")]
    pub fn alkanes_simulate(
        &self,
        block: u128,
        tx: u128,
        inputs: &[u128],
    ) -> Result<serde_json::Value, ProviderError> {
        match &self.sync {
            ChainSource::Subfrost(c) => {
                c.check_network(self.network)?;
                c.alkanes_simulate(block, tx, inputs)
            }
            ChainSource::Mock(m) => m.alkanes_simulate(),
            ChainSource::Esplora(_) => Err(self.no_alkanes()),
        }
    }
}

/// Ceiling on a believable fee estimate. Far above any historical
/// mempool peak, far below what a cast from a hostile float could reach.
pub const MAX_FEE_RATE_SAT_VB: f64 = 10_000.0;

/// Largest conf target ≤ the requested one; else the closest above.
///
/// The estimate map is remote data. A NaN casts to 0 (silently
/// under-fees to the 1 sat/vB floor) and an infinite or huge value
/// saturates to `u32::MAX` (a fee-burn); both are refused instead of
/// cast, so the endpoint cannot choose the fee outside sane bounds.
pub fn pick_fee_rate(estimates: &HashMap<u16, f64>, target: u16) -> Result<FeeRate, String> {
    let sat_vb = estimates
        .iter()
        .filter(|(k, _)| **k <= target)
        .max_by_key(|(k, _)| **k)
        .or_else(|| estimates.iter().min_by_key(|(k, _)| **k))
        .map(|(_, rate)| *rate)
        .unwrap_or(1.0);
    if !sat_vb.is_finite() || !(0.0..=MAX_FEE_RATE_SAT_VB).contains(&sat_vb) {
        return Err(format!(
            "provider returned an unusable fee rate: {sat_vb} sat/vB"
        ));
    }
    Ok(FeeRate::from_sat_per_vb_u32((sat_vb.ceil() as u32).max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EsploraConfig, RedactedUrl, SubfrostConfig};

    fn with_key(key: &str) -> Config {
        Config {
            subfrost: Some(SubfrostConfig {
                api_key: key.into(),
            }),
            ..Config::default()
        }
    }

    fn setup_reason(err: ProviderError) -> String {
        match err {
            ProviderError::Setup { reason, .. } => reason,
            other => panic!("expected a setup error, got {other}"),
        }
    }

    /// The fee-estimate map is remote data: values that would cast into a
    /// wrong fee (NaN → 0, ∞ → u32::MAX) are refused, sane ones round up.
    #[test]
    fn hostile_fee_estimates_are_rejected_not_cast() {
        for hostile in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1e12] {
            let estimates = HashMap::from([(2u16, hostile)]);
            assert!(
                pick_fee_rate(&estimates, 2).is_err(),
                "{hostile} sat/vB must be refused"
            );
        }
        let sane = HashMap::from([(2u16, 3.5)]);
        assert_eq!(
            pick_fee_rate(&sane, 2).unwrap(),
            FeeRate::from_sat_per_vb_u32(4)
        );
        // An empty map keeps the historical 1 sat/vB floor.
        assert_eq!(
            pick_fee_rate(&HashMap::new(), 2).unwrap(),
            FeeRate::from_sat_per_vb_u32(1)
        );
        // At the ceiling: usable; the floor still applies to tiny rates.
        assert_eq!(
            pick_fee_rate(&HashMap::from([(2u16, MAX_FEE_RATE_SAT_VB)]), 2).unwrap(),
            FeeRate::from_sat_per_vb_u32(10_000)
        );
        assert_eq!(
            pick_fee_rate(&HashMap::from([(2u16, 0.0)]), 2).unwrap(),
            FeeRate::from_sat_per_vb_u32(1)
        );
        // Human policy selects the long target from a provider map without
        // giving the agent control over the rate.
        let testnet = HashMap::from([(2u16, 211.591), (1008u16, 1.0)]);
        assert_eq!(
            pick_fee_rate(&testnet, 1008).unwrap(),
            FeeRate::from_sat_per_vb_u32(1)
        );
    }

    #[test]
    fn defaults_need_no_configuration() {
        let config = Config::default();
        let signet = overview(&config, None, Network::Signet).unwrap();
        assert_eq!(signet.provider, "mempool");
        assert_eq!(signet.url, "https://mempool.space");
        assert_eq!(signet.source, Source::Default);
        let services = resolve(&config, None, Network::Signet).unwrap();
        assert_eq!(services.fee_target_blocks, 2);
        // The display shows the origin; the transport gets the full path.
        match &services.sync {
            ChainSource::Esplora(e) => assert_eq!(e.url(), "https://mempool.space/signet/api"),
            other => panic!("expected Esplora, got {other:?}"),
        }

        // mempool.space serves no regtest: the default is a local Esplora.
        let regtest = overview(&config, None, Network::Regtest).unwrap();
        assert_eq!(regtest.provider, "esplora");
        assert_eq!(regtest.url, "http://localhost:3002");
    }

    #[test]
    fn fee_target_is_human_policy_scoped_to_the_network() {
        let mut config = Config::default();
        config.signet.fee_target = Some(1008);
        config.mainnet.fee_target = Some(6);
        assert_eq!(
            resolve(&config, None, Network::Signet)
                .unwrap()
                .fee_target_blocks,
            1008
        );
        assert_eq!(
            resolve(&config, None, Network::Bitcoin)
                .unwrap()
                .fee_target_blocks,
            6
        );
        for target in [0, 1009] {
            config.signet.fee_target = Some(target);
            assert!(resolve(&config, None, Network::Signet).is_err());
        }
    }

    #[test]
    fn a_saved_subfrost_key_serves_the_chosen_networks() {
        let mut config = with_key("APIKEYSECRET");
        config.signet.chain = Some(ChainChoice::Subfrost);
        let signet = overview(&config, None, Network::Signet).unwrap();
        assert_eq!(signet.provider, "subfrost");
        assert_eq!(signet.url, "https://signet.subfrost.io");
        assert_eq!(signet.auth, AuthKind::ApiKey);
        assert!(!format!("{signet:?}").contains("APIKEYSECRET"));
        assert!(matches!(
            resolve(&config, None, Network::Signet).unwrap().sync,
            ChainSource::Subfrost(_)
        ));

        // The key alone changes no network's provider.
        let mainnet = overview(&config, None, Network::Bitcoin).unwrap();
        assert_eq!(mainnet.provider, "mempool");

        // Subfrost publishes no regtest endpoint: not set up there.
        config.regtest.chain = Some(ChainChoice::Subfrost);
        let reason = setup_reason(resolve(&config, None, Network::Regtest).unwrap_err());
        assert!(reason.contains("sats providers add subfrost"), "{reason}");
        config.regtest.subfrost_url = Some(RedactedUrl("http://localhost:18888/KEY".into()));
        let regtest = overview(&config, None, Network::Regtest).unwrap();
        assert_eq!(regtest.url, "http://localhost:18888");
    }

    #[test]
    fn choices_that_cannot_be_served_are_setup_errors() {
        let mut config = Config::default();
        config.signet.chain = Some(ChainChoice::Subfrost);
        let reason = setup_reason(resolve(&config, None, Network::Signet).unwrap_err());
        assert!(reason.contains("Subfrost isn't set up"), "{reason}");

        let mut config = Config::default();
        config.signet.chain = Some(ChainChoice::Esplora);
        let reason = setup_reason(resolve(&config, None, Network::Signet).unwrap_err());
        assert!(reason.contains("no Esplora URL"), "{reason}");

        let mut config = Config::default();
        config.regtest.chain = Some(ChainChoice::Mempool);
        let reason = setup_reason(resolve(&config, None, Network::Regtest).unwrap_err());
        assert!(reason.contains("mempool.space"), "{reason}");
    }

    #[test]
    fn an_override_replaces_the_provider_for_one_run() {
        let mut config = with_key("SAVEDKEY");
        config.signet.esplora = Some(EsploraConfig {
            url: RedactedUrl("https://bitcoin.example/api".into()),
            bearer: Some("t".into()),
        });
        config.signet.chain = Some(ChainChoice::Esplora);
        match &resolve(&config, None, Network::Signet).unwrap().sync {
            ChainSource::Esplora(e) => assert_eq!(e.url(), "https://bitcoin.example/api"),
            other => panic!("expected Esplora, got {other:?}"),
        }
        let cli = parse_cli_provider("subfrost=https://other.example/v4/jsonrpc").unwrap();
        let view = overview(&config, Some(&cli), Network::Signet).unwrap();
        assert_eq!(view.source, Source::Override);
        assert_eq!(view.url, "https://other.example");
        assert_eq!(
            view.auth,
            AuthKind::None,
            "the saved key never follows an override"
        );
    }

    #[test]
    fn file_endpoints_select_the_test_driver() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file://{}", dir.path().display());
        let mut config = Config::default();
        config.signet.chain = Some(ChainChoice::Esplora);
        config.signet.esplora = Some(EsploraConfig {
            url: RedactedUrl(url),
            bearer: None,
        });
        let services = resolve(&config, None, Network::Signet).unwrap();
        assert!(matches!(services.sync, ChainSource::Mock(_)));

        std::fs::write(dir.path().join("broadcast-via"), "http://127.0.0.1:9").unwrap();
        let services = resolve(&config, None, Network::Signet).unwrap();
        assert!(matches!(services.fees, ChainSource::Mock(_)));
        assert!(matches!(services.broadcast, ChainSource::Esplora(_)));
    }

    #[test]
    #[cfg(feature = "experimental-alkanes")]
    fn alkanes_need_subfrost_as_the_provider() {
        // A saved key alone serves nothing: Subfrost must be the network's
        // provider, so no call reaches it here.
        for config in [Config::default(), with_key("k")] {
            let services = resolve(&config, None, Network::Signet).unwrap();
            let err = services.alkanes_bytecode(2, 1).unwrap_err();
            assert!(
                err.to_string().contains("sats providers add subfrost"),
                "{err}"
            );
        }
    }

    #[test]
    fn cli_provider_grammar() {
        let p = parse_cli_provider("esplora=https://mempool.space/api").unwrap();
        assert_eq!(p.kind, DriverKind::Esplora);
        assert_eq!(p.url, "https://mempool.space/api");
        // Only the first '=' splits: path keys containing '=' survive.
        let p = parse_cli_provider("subfrost=https://x.example/v4/a=b/jsonrpc").unwrap();
        assert_eq!(p.url, "https://x.example/v4/a=b/jsonrpc");
        assert!(parse_cli_provider("mempool.space").is_err());
        assert!(parse_cli_provider("carrier-pigeon=http://x").is_err());
        assert!(parse_cli_provider("mock=file:///x").is_err());
        assert!(parse_cli_provider("esplora=").is_err());
    }
}
