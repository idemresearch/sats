//! Typed chain providers advertising capabilities.
//!
//! A provider is an endpoint bound to one network, advertising one or more
//! capabilities: chain access (`chain.sync`, `chain.fees`, `chain.broadcast`)
//! and metaprotocol UTXO guards (`guard.ord`, `guard.alkanes`,
//! `guard.native`). Drivers map real APIs onto those capabilities as audited
//! enums — no plugin surface. `sats-core` never sees any of this: it receives
//! only facts (outpoints to avoid).
//!
//! Resolution is pure config work (no network IO) and tiered, most to least
//! specific: CLI `--provider` overrides (which replace the whole set for the
//! active network) → `[providers.*]` config → the legacy `[esplora]` map →
//! built-in defaults. Guards never come from legacy or built-in tiers: sats
//! ships no default indexer, ever.

pub mod error;
pub mod esplora;
pub mod guards;
pub mod mock;
pub mod subfrost;

use std::collections::{BTreeSet, HashMap};

use sats_core::bitcoin::{FeeRate, Network, Transaction, Txid};

use crate::config::{Config, network_name, parse_network};
use crate::walletd::WalletCtx;

pub use error::ProviderError;
pub use guards::{GuardReport, UtxoGuard};

use esplora::EsploraProvider;
use mock::MockProvider;
use subfrost::SubfrostClient;

/// Per HTTP request, not a deadline for an entire multi-request wallet scan.
pub const HTTP_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    ChainSync,
    ChainFees,
    ChainBroadcast,
    GuardOrd,
    GuardAlkanes,
    GuardNative,
    /// Alkanes contract views: bytecode fetch and call simulation. Read
    /// only — a view can inform a human, never authorize a spend.
    AlkanesView,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::ChainSync => "chain.sync",
            Capability::ChainFees => "chain.fees",
            Capability::ChainBroadcast => "chain.broadcast",
            Capability::GuardOrd => "guard.ord",
            Capability::GuardAlkanes => "guard.alkanes",
            Capability::GuardNative => "guard.native",
            Capability::AlkanesView => "alkanes.view",
        }
    }

    pub fn is_guard(self) -> bool {
        matches!(
            self,
            Capability::GuardOrd | Capability::GuardAlkanes | Capability::GuardNative
        )
    }

    /// Expand a config token: exact capability names plus the group aliases
    /// "chain" and "guard".
    fn expand(token: &str) -> Option<Vec<Capability>> {
        Some(match token {
            "chain" => vec![
                Capability::ChainSync,
                Capability::ChainFees,
                Capability::ChainBroadcast,
            ],
            "chain.sync" => vec![Capability::ChainSync],
            "chain.fees" => vec![Capability::ChainFees],
            "chain.broadcast" => vec![Capability::ChainBroadcast],
            "guard" => vec![
                Capability::GuardOrd,
                Capability::GuardAlkanes,
                Capability::GuardNative,
            ],
            "guard.ord" => vec![Capability::GuardOrd],
            "guard.alkanes" => vec![Capability::GuardAlkanes],
            "guard.native" => vec![Capability::GuardNative],
            // Exact name only, deliberately outside the "guard" alias: a
            // view reads contracts, a guard protects UTXOs.
            "alkanes.view" => vec![Capability::AlkanesView],
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverKind {
    Esplora,
    Subfrost,
    Mock,
}

impl DriverKind {
    fn from_str(s: &str) -> Option<DriverKind> {
        Some(match s {
            "esplora" => DriverKind::Esplora,
            "subfrost" => DriverKind::Subfrost,
            "mock" => DriverKind::Mock,
            _ => return None,
        })
    }

    /// The audited capability table: what each driver can do at most.
    fn supported_caps(self) -> &'static [Capability] {
        match self {
            DriverKind::Esplora => &[
                Capability::ChainSync,
                Capability::ChainFees,
                Capability::ChainBroadcast,
            ],
            DriverKind::Subfrost => &[
                Capability::ChainSync,
                Capability::ChainFees,
                Capability::ChainBroadcast,
                Capability::GuardOrd,
                Capability::GuardAlkanes,
                Capability::AlkanesView,
            ],
            DriverKind::Mock => &[
                Capability::ChainSync,
                Capability::ChainFees,
                Capability::ChainBroadcast,
                Capability::GuardNative,
                Capability::AlkanesView,
            ],
        }
    }

    /// Capabilities enabled when configuration omits an explicit filter.
    /// Subfrost is a chain provider by default; indexed protocol behavior is
    /// opt-in even though the driver implements it.
    fn default_caps(self) -> &'static [Capability] {
        match self {
            DriverKind::Subfrost => &[
                Capability::ChainSync,
                Capability::ChainFees,
                Capability::ChainBroadcast,
            ],
            _ => self.supported_caps(),
        }
    }
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
    let kind = DriverKind::from_str(kind)
        .ok_or_else(|| "unknown provider kind (use esplora or subfrost)".to_string())?;
    if url.is_empty() {
        return Err("expected KIND=URL with a non-empty url".to_string());
    }
    Ok(CliProvider {
        kind,
        url: url.to_string(),
    })
}

/// Where a provider entry came from, most to least specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A `--provider` value on the command line.
    Override,
    /// A `[providers.<name>]` config entry.
    Config,
    /// The legacy `[esplora]` network-to-URL map.
    Legacy,
    /// The built-in Esplora endpoint for the network.
    Default,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Override => "override",
            Source::Config => "config",
            Source::Legacy => "legacy",
            Source::Default => "default",
        }
    }

    /// Explicit entries win every capability they offer; the fallback
    /// tiers only fill chain capabilities nothing explicit offers.
    fn is_explicit(self) -> bool {
        matches!(self, Source::Override | Source::Config)
    }
}

/// Which credential, if any, a provider sends. Never the credential.
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

/// A validated provider entry: driver, network binding, capabilities.
struct ProviderSpec {
    name: String,
    source: Source,
    auth: AuthKind,
    driver: Driver,
    caps: BTreeSet<Capability>,
}

/// The instantiated transport for one provider, shared by every service
/// slot the provider fills.
#[derive(Debug, Clone)]
enum Driver {
    Esplora(EsploraProvider),
    Subfrost(SubfrostClient),
    Mock(MockProvider),
}

impl Driver {
    fn kind(&self) -> DriverKind {
        match self {
            Driver::Esplora(_) => DriverKind::Esplora,
            Driver::Subfrost(_) => DriverKind::Subfrost,
            Driver::Mock(_) => DriverKind::Mock,
        }
    }

    fn display_url(&self) -> &str {
        match self {
            Driver::Esplora(e) => e.display_url(),
            Driver::Subfrost(c) => c.display_url(),
            Driver::Mock(m) => m.display_url(),
        }
    }
}

#[derive(Debug)]
pub enum ChainSource {
    Esplora(EsploraProvider),
    Subfrost(SubfrostClient),
    Mock(MockProvider),
}

#[derive(Debug)]
pub enum FeeSource {
    Esplora(EsploraProvider),
    Subfrost(SubfrostClient),
    Mock(MockProvider),
}

#[derive(Debug)]
pub enum BroadcastSource {
    Esplora(EsploraProvider),
    Subfrost(SubfrostClient),
    Mock(MockProvider),
}

#[derive(Debug)]
pub enum AlkanesSource {
    Subfrost(SubfrostClient),
    Mock(MockProvider),
}

/// The resolved service set for one network: at most one source per chain
/// capability, at most one alkanes view, any number of guards.
#[derive(Debug)]
pub struct Services {
    network: Network,
    fee_target_blocks: u16,
    sync: Option<ChainSource>,
    fees: Option<FeeSource>,
    broadcast: Option<BroadcastSource>,
    alkanes: Option<AlkanesSource>,
    guards: Vec<UtxoGuard>,
}

/// Every candidate provider for one network, and which one fills each
/// single-provider slot (an index into `specs`). Guards are every
/// explicit spec's guard capabilities.
struct Selection {
    fee_target_blocks: u16,
    specs: Vec<ProviderSpec>,
    sync: Option<usize>,
    fees: Option<usize>,
    broadcast: Option<usize>,
    alkanes: Option<usize>,
}

impl Selection {
    /// The capabilities a spec actually serves in this resolution.
    fn serves(&self, index: usize) -> BTreeSet<Capability> {
        let spec = &self.specs[index];
        let mut caps: BTreeSet<Capability> = [
            (self.sync, Capability::ChainSync),
            (self.fees, Capability::ChainFees),
            (self.broadcast, Capability::ChainBroadcast),
            (self.alkanes, Capability::AlkanesView),
        ]
        .into_iter()
        .filter(|(slot, _)| *slot == Some(index))
        .map(|(_, cap)| cap)
        .collect();
        if spec.source.is_explicit() {
            caps.extend(
                spec.caps
                    .iter()
                    .filter(|c| c.is_guard() && guard_for(&spec.driver, **c).is_some()),
            );
        }
        caps
    }
}

/// Resolve the provider set for `network` from CLI overrides and config.
/// Pure: no network IO. Call when a workflow reaches chain-dependent work.
pub fn resolve(
    config: &Config,
    cli: &[CliProvider],
    network: Network,
) -> Result<Services, ProviderError> {
    let selection = select(config, cli, network)?;
    let slot = |index: Option<usize>| index.map(|i| &selection.specs[i].driver);

    let guards = selection
        .specs
        .iter()
        .filter(|s| s.source.is_explicit())
        .flat_map(|s| {
            s.caps
                .iter()
                .filter(|c| c.is_guard())
                .filter_map(|c| guard_for(&s.driver, *c))
                .collect::<Vec<_>>()
        })
        .collect();

    Ok(Services {
        network,
        fee_target_blocks: selection.fee_target_blocks,
        sync: slot(selection.sync).map(|d| match d {
            Driver::Esplora(e) => ChainSource::Esplora(e.clone()),
            Driver::Subfrost(c) => ChainSource::Subfrost(c.clone()),
            Driver::Mock(m) => ChainSource::Mock(m.clone()),
        }),
        fees: slot(selection.fees).map(|d| match d {
            Driver::Esplora(e) => FeeSource::Esplora(e.clone()),
            Driver::Subfrost(c) => FeeSource::Subfrost(c.clone()),
            Driver::Mock(m) => FeeSource::Mock(m.clone()),
        }),
        broadcast: slot(selection.broadcast).map(|d| match d {
            Driver::Esplora(e) => BroadcastSource::Esplora(e.clone()),
            Driver::Subfrost(c) => BroadcastSource::Subfrost(c.clone()),
            Driver::Mock(m) => BroadcastSource::Mock(m.clone()),
        }),
        alkanes: slot(selection.alkanes).and_then(|d| match d {
            Driver::Subfrost(c) => Some(AlkanesSource::Subfrost(c.clone())),
            Driver::Mock(m) => Some(AlkanesSource::Mock(m.clone())),
            Driver::Esplora(_) => None,
        }),
        guards,
    })
}

/// One provider as `sats providers` shows it: display-safe fields only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInfo {
    pub name: String,
    pub driver: &'static str,
    /// Redacted origin, never the configured URL.
    pub url: String,
    pub source: Source,
    pub auth: AuthKind,
    /// What this provider is used for in the current resolution. Empty
    /// for an entry another provider shadows.
    pub serves: Vec<Capability>,
}

/// Describe what [`resolve`] would use for `network`: every explicit
/// provider, and a fallback tier only when it fills a capability. Pure,
/// like `resolve`, and fails exactly when `resolve` fails.
pub fn describe(
    config: &Config,
    cli: &[CliProvider],
    network: Network,
) -> Result<Vec<ProviderInfo>, ProviderError> {
    let selection = select(config, cli, network)?;
    Ok(selection
        .specs
        .iter()
        .enumerate()
        .map(|(index, spec)| (spec, selection.serves(index)))
        .filter(|(spec, serves)| spec.source.is_explicit() || !serves.is_empty())
        .map(|(spec, serves)| ProviderInfo {
            name: spec.name.clone(),
            driver: driver_name(spec.driver.kind()),
            url: spec.driver.display_url().to_string(),
            source: spec.source,
            auth: spec.auth,
            serves: serves.into_iter().collect(),
        })
        .collect())
}

/// Confirm that one provider entry answers and serves `network`, before a
/// human relies on it. This is network I/O, unlike resolution.
pub fn check_entry(
    name: &str,
    entry: &crate::config::ProviderConfig,
    network: Network,
) -> Result<(), ProviderError> {
    let kind = DriverKind::from_str(&entry.driver).ok_or_else(|| ProviderError::BadConfig {
        name: name.to_string(),
        reason: format!("unknown driver {:?}", entry.driver),
    })?;
    let spec = build_spec(
        name,
        Source::Config,
        kind,
        &entry.url,
        entry.capabilities.as_deref(),
        entry.api_key.as_deref(),
        entry.auth.as_ref(),
    )?;
    match &spec.driver {
        Driver::Esplora(e) => e.check_network(network),
        Driver::Subfrost(c) => c.check_network(network),
        Driver::Mock(m) => m.sync(),
    }
}

fn select(
    config: &Config,
    cli: &[CliProvider],
    network: Network,
) -> Result<Selection, ProviderError> {
    let net_name = network_name(network);
    let fee_target_blocks = config.fee_targets.get(net_name).copied().unwrap_or(2);
    if !(1..=1008).contains(&fee_target_blocks) {
        return Err(ProviderError::BadConfig {
            name: "fee_targets".into(),
            reason: format!("{net_name} confirmation target must be between 1 and 1008 blocks"),
        });
    }

    // CLI overrides replace the whole provider set for the active network.
    let mut specs: Vec<ProviderSpec> = if !cli.is_empty() {
        cli.iter()
            .map(|p| {
                let name = format!("cli:{}", driver_name(p.kind));
                build_spec(&name, Source::Override, p.kind, &p.url, None, None, None)
            })
            .collect::<Result<_, _>>()?
    } else {
        config
            .providers
            .iter()
            .filter(|(_, p)| p.network == net_name)
            .map(|(name, p)| {
                let kind =
                    DriverKind::from_str(&p.driver).ok_or_else(|| ProviderError::BadConfig {
                        name: name.clone(),
                        reason: format!("unknown driver {:?}", p.driver),
                    })?;
                // The network string must parse even though we filtered by
                // equality — catches typos on entries for other networks too.
                parse_network(&p.network).map_err(|e| ProviderError::BadConfig {
                    name: name.clone(),
                    reason: e.to_string(),
                })?;
                build_spec(
                    name,
                    Source::Config,
                    kind,
                    &p.url,
                    p.capabilities.as_deref(),
                    p.api_key.as_deref(),
                    p.auth.as_ref(),
                )
            })
            .collect::<Result<_, _>>()?
    };

    // Fallback for chain capabilities only: the legacy [esplora] map, then
    // built-in defaults. Guards never fall back, and CLI overrides replace
    // everything — no fallback behind an explicit --provider.
    if cli.is_empty()
        && let Some(fallback) = legacy_or_default_esplora(config, net_name)?
    {
        specs.push(fallback);
    }

    // Per capability: explicit providers win; the fallback esplora covers
    // any chain capability no explicit provider offers.
    let pick = |cap: Capability, prefer: Option<usize>| -> Result<Option<usize>, ProviderError> {
        let offers = |s: &ProviderSpec| s.caps.contains(&cap);
        let candidates: Vec<usize> = (0..specs.len())
            .filter(|&i| specs[i].source.is_explicit() && offers(&specs[i]))
            .collect();
        if candidates.is_empty() {
            return Ok(
                (0..specs.len()).find(|&i| !specs[i].source.is_explicit() && offers(&specs[i]))
            );
        }
        if let Some(preferred) = prefer
            && candidates.contains(&preferred)
        {
            return Ok(Some(preferred));
        }
        match candidates.len() {
            1 => Ok(Some(candidates[0])),
            _ => Err(ProviderError::Ambiguous {
                cap: cap.as_str(),
                network: net_name,
                names: candidates
                    .iter()
                    .map(|&i| specs[i].name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            }),
        }
    };

    let sync = pick(Capability::ChainSync, None)?;
    let fees = pick(Capability::ChainFees, sync)?;
    let broadcast = pick(Capability::ChainBroadcast, sync)?;
    // Never from the fallback tiers: like guards, an alkanes view is an
    // explicit trust decision (the esplora fallback cannot offer it).
    let alkanes = pick(Capability::AlkanesView, sync)?;

    Ok(Selection {
        fee_target_blocks,
        specs,
        sync,
        fees,
        broadcast,
        alkanes,
    })
}

pub fn driver_name(kind: DriverKind) -> &'static str {
    match kind {
        DriverKind::Esplora => "esplora",
        DriverKind::Subfrost => "subfrost",
        DriverKind::Mock => "mock",
    }
}

fn build_spec(
    name: &str,
    source: Source,
    kind: DriverKind,
    url: &str,
    cap_filter: Option<&[String]>,
    api_key: Option<&str>,
    auth: Option<&crate::config::AuthConfig>,
) -> Result<ProviderSpec, ProviderError> {
    let bad = |reason: String| ProviderError::BadConfig {
        name: name.to_string(),
        reason,
    };
    let mut caps: BTreeSet<Capability> = match cap_filter {
        Some(_) => kind.supported_caps(),
        None => kind.default_caps(),
    }
    .iter()
    .copied()
    .collect();
    if let Some(filter) = cap_filter {
        let mut wanted = BTreeSet::new();
        for token in filter {
            let expanded = Capability::expand(token)
                .ok_or_else(|| bad(format!("unknown capability {token:?}")))?;
            wanted.extend(expanded);
        }
        caps.retain(|c| wanted.contains(c));
        if caps.is_empty() {
            return Err(bad(
                "capabilities filter leaves nothing this driver offers".into()
            ));
        }
    }
    let driver = match kind {
        DriverKind::Esplora => {
            if api_key.is_some() {
                return Err(bad(
                    "api_key authentication is only valid for subfrost".into()
                ));
            }
            Driver::Esplora(EsploraProvider::new(
                name.to_string(),
                url.to_string(),
                auth.and_then(|a| a.bearer.clone()),
            ))
        }
        DriverKind::Subfrost => {
            if auth.is_some() {
                return Err(bad(
                    "auth configuration is only valid for esplora; put api_key directly under the subfrost provider".into(),
                ));
            }
            if api_key.is_some_and(|key| key.trim().is_empty()) {
                return Err(bad("subfrost api_key must not be empty".into()));
            }
            Driver::Subfrost(SubfrostClient::new(
                url.to_string(),
                api_key.map(str::to_owned),
            ))
        }
        DriverKind::Mock => {
            if api_key.is_some() || auth.is_some() {
                return Err(bad("authentication is not valid for mock providers".into()));
            }
            Driver::Mock(MockProvider::new(url).map_err(bad)?)
        }
    };
    let auth = if api_key.is_some() {
        AuthKind::ApiKey
    } else if auth.and_then(|a| a.bearer.as_ref()).is_some() {
        AuthKind::Bearer
    } else {
        AuthKind::None
    };
    Ok(ProviderSpec {
        name: name.to_string(),
        source,
        auth,
        driver,
        caps,
    })
}

fn guard_for(driver: &Driver, cap: Capability) -> Option<UtxoGuard> {
    match (driver, cap) {
        (Driver::Subfrost(c), Capability::GuardOrd) => Some(UtxoGuard::SubfrostOrd(c.clone())),
        (Driver::Subfrost(c), Capability::GuardAlkanes) => {
            Some(UtxoGuard::SubfrostAlkanes(c.clone()))
        }
        (Driver::Mock(m), Capability::GuardNative) => Some(UtxoGuard::Mock(m.clone())),
        _ => None,
    }
}

/// The legacy `[esplora]` map, else built-in defaults — chain caps only.
fn legacy_or_default_esplora(
    config: &Config,
    net_name: &'static str,
) -> Result<Option<ProviderSpec>, ProviderError> {
    let (name, source, url) = match config.esplora.get(net_name) {
        Some(url) => ("esplora(legacy)", Source::Legacy, url.clone()),
        None => match Config::builtin_esplora_url(net_name) {
            Some(url) => ("esplora(default)", Source::Default, url),
            None => return Ok(None),
        },
    };
    Ok(Some(build_spec(
        name,
        source,
        DriverKind::Esplora,
        &url,
        None,
        None,
        None,
    )?))
}

impl Services {
    fn net_name(&self) -> &'static str {
        network_name(self.network)
    }

    fn no_provider(&self, cap: Capability) -> ProviderError {
        ProviderError::NoProvider {
            cap: cap.as_str(),
            network: self.net_name(),
        }
    }

    /// Sync the wallet: a full scan on first touch, incremental after.
    /// Validates the provider serves the wallet's network first.
    pub fn sync_wallet(&self, ctx: &mut WalletCtx) -> Result<(), ProviderError> {
        let source = self
            .sync
            .as_ref()
            .ok_or_else(|| self.no_provider(Capability::ChainSync))?;
        let status = crate::ui::StatusLine::start("syncing…");
        let result = self.sync_inner(source, ctx);
        status.finish();
        result
    }

    fn sync_inner(&self, source: &ChainSource, ctx: &mut WalletCtx) -> Result<(), ProviderError> {
        let sync_err = |url: &str, message: String| ProviderError::Sync {
            url: url.to_string(),
            message,
        };
        match source {
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
        let source = self
            .fees
            .as_ref()
            .ok_or_else(|| self.no_provider(Capability::ChainFees))?;
        let (estimates, url) = match source {
            FeeSource::Esplora(e) => (e.fee_estimates()?, e.display_url().to_string()),
            FeeSource::Subfrost(c) => (c.fee_estimates()?, c.display_url().to_string()),
            FeeSource::Mock(m) => (m.fee_estimates()?, "mock".to_string()),
        };
        pick_fee_rate(&estimates, self.fee_target_blocks)
            .map_err(|message| ProviderError::Fees { url, message })
    }

    /// Broadcast and record the transaction as unconfirmed in the wallet.
    pub fn broadcast(&self, ctx: &mut WalletCtx, tx: &Transaction) -> Result<Txid, ProviderError> {
        let txid = match self
            .broadcast
            .as_ref()
            .ok_or_else(|| self.no_provider(Capability::ChainBroadcast))?
        {
            BroadcastSource::Esplora(e) => {
                e.broadcast(tx)?;
                tx.compute_txid()
            }
            BroadcastSource::Subfrost(c) => c.broadcast(tx)?,
            BroadcastSource::Mock(m) => m.broadcast(tx)?,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        ctx.wallet.apply_unconfirmed_txs([(tx.clone(), now)]);
        let _ = ctx.persist();
        Ok(txid)
    }

    pub fn has_guards(&self) -> bool {
        !self.guards.is_empty()
    }

    /// Contract bytecode for an alkane id. Validates the endpoint's
    /// network before trusting its answer.
    pub fn alkanes_bytecode(&self, block: u128, tx: u128) -> Result<Vec<u8>, ProviderError> {
        match self
            .alkanes
            .as_ref()
            .ok_or_else(|| self.no_provider(Capability::AlkanesView))?
        {
            AlkanesSource::Subfrost(c) => {
                c.check_network(self.network)?;
                c.alkanes_bytecode(block, tx)
            }
            AlkanesSource::Mock(m) => m.alkanes_bytecode(block, tx),
        }
    }

    /// Simulate a contract call; the result is the endpoint's verbatim
    /// JSON. Advisory only — a simulation never authorizes anything.
    pub fn alkanes_simulate(
        &self,
        block: u128,
        tx: u128,
        inputs: &[u128],
    ) -> Result<serde_json::Value, ProviderError> {
        match self
            .alkanes
            .as_ref()
            .ok_or_else(|| self.no_provider(Capability::AlkanesView))?
        {
            AlkanesSource::Subfrost(c) => {
                c.check_network(self.network)?;
                c.alkanes_simulate(block, tx, inputs)
            }
            AlkanesSource::Mock(m) => m.alkanes_simulate(),
        }
    }

    /// Ask every configured guard which of these outpoints are protected.
    /// Fail-closed: any guard erroring errors the whole query.
    pub fn protected_outpoints(
        &self,
        outpoints: &[sats_core::bitcoin::OutPoint],
    ) -> Result<GuardReport, ProviderError> {
        guards::protected_outpoints(&self.guards, outpoints)
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
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{AuthConfig, ProviderConfig};

    fn provider(driver: &str, network: &str, url: &str, caps: Option<Vec<&str>>) -> ProviderConfig {
        ProviderConfig {
            driver: driver.into(),
            network: network.into(),
            url: url.into(),
            capabilities: caps.map(|c| c.into_iter().map(String::from).collect()),
            api_key: None,
            auth: None,
        }
    }

    fn config_with(providers: BTreeMap<String, ProviderConfig>) -> Config {
        Config {
            network: "signet".into(),
            fee_targets: BTreeMap::new(),
            esplora: BTreeMap::new(),
            providers,
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
    fn defaults_resolve_when_nothing_configured() {
        let services = resolve(&config_with(BTreeMap::new()), &[], Network::Signet).unwrap();
        assert_eq!(services.fee_target_blocks, 2);
        assert!(services.sync.is_some());
        assert!(services.fees.is_some());
        assert!(services.broadcast.is_some());
        assert!(!services.has_guards());
    }

    #[test]
    fn fee_target_is_human_policy_scoped_to_the_active_network() {
        let mut config = config_with(BTreeMap::new());
        config.fee_targets.insert("signet".into(), 1008);
        config.fee_targets.insert("mainnet".into(), 6);

        let signet = resolve(&config, &[], Network::Signet).unwrap();
        let mainnet = resolve(&config, &[], Network::Bitcoin).unwrap();
        assert_eq!(signet.fee_target_blocks, 1008);
        assert_eq!(mainnet.fee_target_blocks, 6);
    }

    #[test]
    fn unusable_fee_target_is_rejected() {
        for target in [0, 1009] {
            let mut config = config_with(BTreeMap::new());
            config.fee_targets.insert("signet".into(), target);
            assert!(matches!(
                resolve(&config, &[], Network::Signet),
                Err(ProviderError::BadConfig { .. })
            ));
        }
    }

    #[test]
    fn legacy_esplora_map_beats_builtin() {
        let mut config = config_with(BTreeMap::new());
        config
            .esplora
            .insert("signet".into(), "http://legacy.example/api".into());
        let services = resolve(&config, &[], Network::Signet).unwrap();
        match services.sync.unwrap() {
            ChainSource::Esplora(e) => assert_eq!(e.url(), "http://legacy.example/api"),
            _ => panic!("expected esplora"),
        }
    }

    #[test]
    fn configured_provider_beats_legacy() {
        let mut config = config_with(BTreeMap::from([(
            "mine".into(),
            provider("esplora", "signet", "http://mine.example/api", None),
        )]));
        config
            .esplora
            .insert("signet".into(), "http://legacy.example/api".into());
        let services = resolve(&config, &[], Network::Signet).unwrap();
        match services.sync.unwrap() {
            ChainSource::Esplora(e) => assert_eq!(e.url(), "http://mine.example/api"),
            _ => panic!("expected esplora"),
        }
    }

    #[test]
    fn two_sync_providers_are_ambiguous() {
        let config = config_with(BTreeMap::from([
            (
                "a".into(),
                provider("esplora", "signet", "http://a.example", None),
            ),
            (
                "b".into(),
                provider("esplora", "signet", "http://b.example", None),
            ),
        ]));
        let err = resolve(&config, &[], Network::Signet).unwrap_err();
        assert!(matches!(
            err,
            ProviderError::Ambiguous {
                cap: "chain.sync",
                ..
            }
        ));
    }

    #[test]
    fn capability_filter_disambiguates() {
        let config = config_with(BTreeMap::from([
            (
                "chain".into(),
                provider("esplora", "signet", "http://a.example", None),
            ),
            (
                "fees-only".into(),
                provider(
                    "esplora",
                    "signet",
                    "http://b.example",
                    Some(vec!["chain.fees"]),
                ),
            ),
        ]));
        // Both offer chain.fees; the sync provider is preferred for fees.
        let services = resolve(&config, &[], Network::Signet).unwrap();
        match services.fees.unwrap() {
            FeeSource::Esplora(e) => assert_eq!(e.url(), "http://a.example"),
            _ => panic!("expected esplora"),
        }
    }

    #[test]
    fn other_network_providers_are_ignored() {
        let config = config_with(BTreeMap::from([(
            "mainnet-only".into(),
            provider("esplora", "mainnet", "http://main.example", None),
        )]));
        let services = resolve(&config, &[], Network::Signet).unwrap();
        // Falls through to the built-in signet default.
        match services.sync.unwrap() {
            ChainSource::Esplora(e) => assert!(e.url().contains("signet")),
            _ => panic!("expected esplora"),
        }
    }

    #[test]
    fn unknown_driver_is_rejected() {
        let config = config_with(BTreeMap::from([(
            "weird".into(),
            provider("carrier-pigeon", "signet", "http://x.example", None),
        )]));
        let err = resolve(&config, &[], Network::Signet).unwrap_err();
        assert!(matches!(err, ProviderError::BadConfig { .. }));
    }

    #[test]
    fn unknown_capability_is_rejected() {
        let config = config_with(BTreeMap::from([(
            "weird".into(),
            provider(
                "esplora",
                "signet",
                "http://x.example",
                Some(vec!["chain.teleport"]),
            ),
        )]));
        let err = resolve(&config, &[], Network::Signet).unwrap_err();
        assert!(matches!(err, ProviderError::BadConfig { .. }));
    }

    #[test]
    fn cli_provider_replaces_config() {
        let config = config_with(BTreeMap::from([(
            "mine".into(),
            provider("esplora", "signet", "http://mine.example/api", None),
        )]));
        let cli = vec![CliProvider {
            kind: DriverKind::Esplora,
            url: "http://cli.example/api".into(),
        }];
        let services = resolve(&config, &cli, Network::Signet).unwrap();
        match services.sync.unwrap() {
            ChainSource::Esplora(e) => assert_eq!(e.url(), "http://cli.example/api"),
            _ => panic!("expected esplora"),
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
        assert!(parse_cli_provider("esplora=").is_err());
    }

    #[test]
    fn two_cli_sync_providers_are_ambiguous() {
        let cli = vec![
            CliProvider {
                kind: DriverKind::Esplora,
                url: "http://a.example".into(),
            },
            CliProvider {
                kind: DriverKind::Subfrost,
                url: "http://b.example".into(),
            },
        ];
        let err = resolve(&config_with(BTreeMap::new()), &cli, Network::Signet).unwrap_err();
        assert!(matches!(
            err,
            ProviderError::Ambiguous {
                cap: "chain.sync",
                ..
            }
        ));
    }

    #[test]
    fn bearer_auth_is_accepted() {
        let config = config_with(BTreeMap::from([(
            "authy".into(),
            ProviderConfig {
                driver: "esplora".into(),
                network: "signet".into(),
                url: "http://a.example".into(),
                capabilities: None,
                api_key: None,
                auth: Some(AuthConfig {
                    bearer: Some("token".into()),
                }),
            },
        )]));
        assert!(resolve(&config, &[], Network::Signet).is_ok());
    }

    #[test]
    fn subfrost_api_key_is_typed_and_driver_specific() {
        let config = config_with(BTreeMap::from([(
            "subfrost".into(),
            ProviderConfig {
                driver: "subfrost".into(),
                network: "signet".into(),
                url: "http://a.example".into(),
                capabilities: None,
                api_key: Some("secret".into()),
                auth: None,
            },
        )]));
        let services = resolve(&config, &[], Network::Signet).unwrap();
        assert!(!services.has_guards());
        assert!(services.alkanes.is_none());
        match services.sync.unwrap() {
            ChainSource::Subfrost(client) => {
                assert!(!format!("{client:?}").contains("secret"));
            }
            _ => panic!("expected subfrost"),
        }

        let mut wrong_driver = config;
        wrong_driver.providers.get_mut("subfrost").unwrap().driver = "esplora".into();
        assert!(matches!(
            resolve(&wrong_driver, &[], Network::Signet),
            Err(ProviderError::BadConfig { .. })
        ));

        let mut empty_key = config_with(BTreeMap::from([(
            "subfrost".into(),
            provider("subfrost", "signet", "http://a.example", None),
        )]));
        empty_key.providers.get_mut("subfrost").unwrap().api_key = Some("  ".into());
        assert!(matches!(
            resolve(&empty_key, &[], Network::Signet),
            Err(ProviderError::BadConfig { .. })
        ));
    }

    #[test]
    fn subfrost_indexed_capabilities_require_explicit_opt_in() {
        let config = config_with(BTreeMap::from([(
            "subfrost".into(),
            provider(
                "subfrost",
                "signet",
                "http://a.example",
                Some(vec!["guard", "alkanes.view"]),
            ),
        )]));
        let services = resolve(&config, &[], Network::Signet).unwrap();
        assert!(services.has_guards());
        assert!(services.alkanes.is_some());
        assert!(matches!(services.sync, Some(ChainSource::Esplora(_))));
    }

    fn serves(info: &ProviderInfo) -> Vec<&'static str> {
        info.serves.iter().map(|c| c.as_str()).collect()
    }

    #[test]
    fn describe_shows_the_default_only_while_it_serves() {
        let rows = describe(&config_with(BTreeMap::new()), &[], Network::Signet).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "esplora(default)");
        assert_eq!(rows[0].source, Source::Default);
        assert_eq!(rows[0].auth, AuthKind::None);
        assert_eq!(
            serves(&rows[0]),
            ["chain.sync", "chain.fees", "chain.broadcast"]
        );

        // A logged-in Subfrost takes every chain capability: the default
        // drops out of the list because it no longer serves anything.
        let mut subfrost = provider("subfrost", "signet", "https://user:pw@a.example/KEY", None);
        subfrost.api_key = Some("APIKEYSECRET".into());
        let config = config_with(BTreeMap::from([("subfrost".into(), subfrost)]));
        let rows = describe(&config, &[], Network::Signet).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "subfrost");
        assert_eq!(rows[0].source, Source::Config);
        assert_eq!(rows[0].auth, AuthKind::ApiKey);
        assert_eq!(rows[0].url, "https://a.example");
        assert!(!format!("{rows:?}").contains("APIKEYSECRET"));
        assert!(!format!("{rows:?}").contains("KEY"));
    }

    #[test]
    fn describe_reports_guards_split_roles_and_shadowed_entries() {
        let mut bearer = provider("esplora", "signet", "http://b.example", Some(vec!["chain"]));
        bearer.auth = Some(AuthConfig {
            bearer: Some("t".into()),
        });
        let config = config_with(BTreeMap::from([
            ("chain".into(), bearer),
            (
                "assets".into(),
                provider(
                    "subfrost",
                    "signet",
                    "http://a.example",
                    Some(vec!["chain.fees", "guard", "alkanes.view"]),
                ),
            ),
        ]));
        let rows = describe(&config, &[], Network::Signet).unwrap();
        let by_name = |name: &str| rows.iter().find(|r| r.name == name).unwrap();
        assert_eq!(by_name("chain").auth, AuthKind::Bearer);
        assert_eq!(
            serves(by_name("chain")),
            ["chain.sync", "chain.fees", "chain.broadcast"]
        );
        // Fees prefer the sync provider, so the guard entry serves only
        // its guards and the view.
        assert_eq!(
            serves(by_name("assets")),
            ["guard.ord", "guard.alkanes", "alkanes.view"]
        );
        assert_eq!(rows.len(), 2, "no default row once chain is covered");

        let shadowed = config_with(BTreeMap::from([
            (
                "main".into(),
                provider("esplora", "signet", "http://b.example", None),
            ),
            (
                "spare".into(),
                provider(
                    "esplora",
                    "signet",
                    "http://c.example",
                    Some(vec!["chain.fees"]),
                ),
            ),
        ]));
        let rows = describe(&shadowed, &[], Network::Signet).unwrap();
        let spare = rows.iter().find(|r| r.name == "spare").unwrap();
        assert!(spare.serves.is_empty(), "explicit entries stay listed");
    }

    #[test]
    fn describe_follows_overrides_and_fails_like_resolve() {
        let cli = vec![parse_cli_provider("esplora=http://o.example").unwrap()];
        let config = config_with(BTreeMap::from([(
            "subfrost".into(),
            provider("subfrost", "signet", "http://a.example", None),
        )]));
        let rows = describe(&config, &cli, Network::Signet).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, Source::Override);
        assert_eq!(rows[0].name, "cli:esplora");

        let ambiguous = config_with(BTreeMap::from([
            (
                "a".into(),
                provider("esplora", "signet", "http://a.example", None),
            ),
            (
                "b".into(),
                provider("subfrost", "signet", "http://b.example", None),
            ),
        ]));
        assert!(matches!(
            describe(&ambiguous, &[], Network::Signet),
            Err(ProviderError::Ambiguous { .. })
        ));
    }
}
