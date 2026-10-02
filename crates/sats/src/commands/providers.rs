//! `sats providers`: where a network's chain data comes from, and whether
//! its assets are protected.
//!
//! Every change is a human trust decision written to the config file. A
//! change that points at an endpoint is resolved and checked against the
//! network before it is saved, so a typo or wrong key never becomes the
//! provider the next send uses. Asset protection is turned on only by an
//! explicit flag, answer, or `protect on`, and never off as a side effect.

use std::io::IsTerminal;

use anyhow::{Context, Result, bail};
use sats_core::bitcoin::Network;
use zeroize::Zeroizing;

use crate::cli::{AddArgs, ChainArg, ProviderArg, Toggle};
use crate::config::{
    ChainChoice, Config, EsploraConfig, RedactedUrl, SubfrostConfig, network_name,
};
use crate::provider::{self, CliProvider, Endpoint, Overview, Source};
use crate::store::Store;
use crate::ui;

pub fn list(
    config: &Config,
    overrides: Option<&CliProvider>,
    network: Network,
    json: bool,
) -> Result<()> {
    show(config, overrides, network, json)
}

pub fn add(
    store: &Store,
    mut config: Config,
    network: Network,
    args: &AddArgs,
    json: bool,
) -> Result<()> {
    let net_name = network_name(network);
    let updated;
    match args.kind {
        ProviderArg::Subfrost => {
            if args.url.is_none()
                && config.net(network).subfrost_url.is_none()
                && provider::subfrost::default_url(network).is_none()
            {
                bail!("Subfrost has no default endpoint for {net_name} — pass --url");
            }
            let current = config.subfrost.as_ref().map(|s| s.api_key.clone());
            updated = current.is_some();
            let api_key = read_credential("Subfrost API key", current, true)?
                .context("no Subfrost API key")?;
            config.subfrost = Some(SubfrostConfig { api_key });
            let net = config.net_mut(network);
            if let Some(url) = &args.url {
                net.subfrost_url = Some(RedactedUrl(url.clone()));
            }
            net.chain = Some(ChainChoice::Subfrost);
            if args.protect
                || (!net.protect_assets
                    && !json
                    && std::io::stdin().is_terminal()
                    && ui::confirm(
                        "also turn on asset protection (Subfrost checks every output before a send)?",
                        false,
                    )?)
            {
                net.protect_assets = true;
            }
        }
        ProviderArg::Esplora => {
            if args.protect {
                bail!(
                    "--protect is a Subfrost option: Esplora can't tell which outputs carry assets"
                );
            }
            let url = args
                .url
                .clone()
                .context("esplora needs an endpoint: pass --url")?;
            let existing = config.net(network).esplora.clone();
            updated = existing.is_some();
            // A stored token is only ever kept for the same server: it must
            // not follow a new URL.
            let current = existing.filter(|e| e.url.0 == url).and_then(|e| e.bearer);
            let bearer = read_credential("bearer token", current, false)?;
            let net = config.net_mut(network);
            net.esplora = Some(EsploraConfig {
                url: RedactedUrl(url),
                bearer,
            });
            net.chain = Some(ChainChoice::Esplora);
        }
    }

    check_and_save(store, &config, network, true)?;
    let name = match args.kind {
        ProviderArg::Subfrost => "subfrost",
        ProviderArg::Esplora => "esplora",
    };
    if !json {
        let verb = if updated { "updated" } else { "added" };
        ui::ok(&format!("{verb} {name}  {net_name}"));
        println!();
    }
    show(&config, None, network, json)
}

pub fn use_chain(
    store: &Store,
    mut config: Config,
    network: Network,
    source: ChainArg,
    json: bool,
) -> Result<()> {
    let choice = match source {
        ChainArg::Mempool => ChainChoice::Mempool,
        ChainArg::Subfrost => ChainChoice::Subfrost,
        ChainArg::Esplora => ChainChoice::Esplora,
    };
    config.net_mut(network).chain = Some(choice);
    check_and_save(store, &config, network, false)?;
    if !json {
        ui::ok(&format!(
            "{} chain data: {}",
            network_name(network),
            choice.as_str()
        ));
        println!();
    }
    show(&config, None, network, json)
}

pub fn protect(
    store: &Store,
    mut config: Config,
    network: Network,
    state: Toggle,
    json: bool,
) -> Result<()> {
    let net_name = network_name(network);
    match state {
        Toggle::On => {
            // Name the missing indexer; any other problem with the setup is
            // reported as itself.
            let before = provider::overview(&config, None, network)
                .context("asset protection was not turned on")?;
            if before.alkanes.is_none() {
                bail!(
                    "asset protection needs Subfrost on {net_name} — run `sats providers add subfrost` first"
                );
            }
            config.net_mut(network).protect_assets = true;
            let services = provider::resolve(&config, None, network)?;
            let status = ui::StatusLine::start(&format!("checking Subfrost on {net_name}…"));
            let checked = services.check_index();
            status.finish();
            checked.context("asset protection was not turned on")?;
            config.save(store)?;
            if !json {
                ui::ok(&format!("asset protection on  {net_name}"));
                ui::dim(
                    "before every send, Subfrost is asked which outputs carry inscriptions, runes, or Alkanes;\n\
                     if it can't answer, the send stops (a human can skip the check once with --no-guards)",
                );
                println!();
            }
        }
        Toggle::Off => {
            config.net_mut(network).protect_assets = false;
            config.save(store)?;
            if !json {
                ui::warn(&format!(
                    "asset protection off  {net_name} — only the 546/330-sat postage check protects inscriptions"
                ));
                println!();
            }
        }
    }
    show(&config, None, network, json)
}

pub fn remove(
    store: &Store,
    mut config: Config,
    network: Network,
    provider: ProviderArg,
    json: bool,
) -> Result<()> {
    let mut reverted = Vec::new();
    match provider {
        ProviderArg::Subfrost => {
            let set_up = config.subfrost.is_some()
                || config
                    .networks()
                    .iter()
                    .any(|(_, net)| net.subfrost_url.is_some());
            if !set_up {
                bail!("Subfrost isn't set up");
            }
            let protected: Vec<&str> = config
                .networks()
                .iter()
                .filter(|(_, net)| net.protect_assets)
                .map(|(name, _)| *name)
                .collect();
            if let Some(first) = protected.first() {
                bail!(
                    "asset protection uses Subfrost on {} — turn it off first: \
                     sats --network {first} providers protect off",
                    protected.join(", ")
                );
            }
            config.subfrost = None;
            for net in [
                Network::Bitcoin,
                Network::Signet,
                Network::Testnet4,
                Network::Regtest,
            ] {
                let settings = config.net_mut(net);
                settings.subfrost_url = None;
                if settings.chain == Some(ChainChoice::Subfrost) {
                    settings.chain = None;
                    reverted.push(net);
                }
            }
        }
        ProviderArg::Esplora => {
            let net = config.net_mut(network);
            if net.esplora.take().is_none() {
                bail!("no Esplora server is set up for {}", network_name(network));
            }
            if net.chain == Some(ChainChoice::Esplora) {
                net.chain = None;
                reverted.push(network);
            }
        }
    }
    config.save(store)?;
    if !json {
        ui::ok(match provider {
            ProviderArg::Subfrost => "removed subfrost and its key",
            ProviderArg::Esplora => "removed esplora and its token",
        });
        for net in reverted {
            ui::warn(&format!(
                "{} chain data is back to the default: {}",
                network_name(net),
                ChainChoice::default_for(net).as_str()
            ));
        }
        println!();
    }
    show(&config, None, network, json)
}

/// Resolve the changed configuration, check the chain source against the
/// network (and the indexer, when protection is on and it differs), then
/// save. Nothing is written unless every check passes.
fn check_and_save(
    store: &Store,
    config: &Config,
    network: Network,
    credential: bool,
) -> Result<()> {
    let net_name = network_name(network);
    let services = provider::resolve(config, None, network).context("nothing was saved")?;
    let status = ui::StatusLine::start(&format!("checking {net_name} chain data…"));
    let mut checked = services.check_chain();
    if checked.is_ok() && config.net(network).protect_assets {
        checked = services.check_index();
    }
    status.finish();
    let hint = if credential {
        "nothing was saved — check the URL and key, then try again"
    } else {
        "nothing was saved"
    };
    checked.context(hint)?;
    config.save(store)
}

/// Hidden entry on a terminal, one line on stdin otherwise. Never a CLI
/// argument: argv and shell history leak. Empty input keeps the current
/// credential; a required one must exist.
fn read_credential(label: &str, current: Option<String>, required: bool) -> Result<Option<String>> {
    let input = if std::io::stdin().is_terminal() {
        let hint = match (&current, required) {
            (Some(_), _) => " (enter keeps the saved one)",
            (None, false) => " (enter for none)",
            (None, true) => "",
        };
        Zeroizing::new(rpassword::prompt_password(format!("{label}{hint}: "))?)
    } else {
        let mut line = Zeroizing::new(String::new());
        std::io::stdin().read_line(&mut line)?;
        line
    };
    let input = input.trim();
    if !input.is_empty() {
        return Ok(Some(input.to_string()));
    }
    match current {
        Some(current) => Ok(Some(current)),
        None if required => bail!("no {label}: enter it at the prompt, or pipe it on stdin"),
        None => Ok(None),
    }
}

/// Print what the network uses. When it can't be resolved, show the
/// settings as written and fail with the resolution error, so the list is
/// where a broken setup gets diagnosed.
fn show(
    config: &Config,
    overrides: Option<&CliProvider>,
    network: Network,
    json: bool,
) -> Result<()> {
    let net_name = network_name(network);
    let view = match provider::overview(config, overrides, network) {
        Ok(view) => view,
        Err(err) => {
            if !json {
                println!("{net_name} providers, as configured");
                println!();
                configured(config, network);
                println!();
            }
            return Err(err.into());
        }
    };
    if json {
        println!("{}", overview_json(net_name, &view));
        return Ok(());
    }

    println!("{net_name} providers");
    println!();
    let chain = &view.chain;
    let note = match chain.source {
        Source::Default => " (default)",
        Source::Override => " (--provider)",
        Source::Config => "",
    };
    let rows = [
        [
            "Chain data".to_string(),
            format!("{}{note}", chain.provider),
            endpoint_detail(chain),
        ],
        match &view.protection {
            Some(endpoint) => [
                "Asset protection".into(),
                "on".into(),
                format!("{} checks every output before a send", endpoint.provider),
            ],
            None => [
                "Asset protection".into(),
                "off".into(),
                "only the 546/330-sat postage check".into(),
            ],
        },
        match &view.alkanes {
            Some(endpoint) => [
                "Alkanes views".into(),
                endpoint.provider.into(),
                endpoint_detail(endpoint),
            ],
            None => ["Alkanes views".into(), "—".into(), "needs Subfrost".into()],
        },
    ];
    table(&rows);
    println!();
    if view.alkanes.is_none() {
        let url = if provider::subfrost::default_url(network).is_some() {
            ""
        } else {
            " --url URL"
        };
        ui::dim(&format!(
            "add Subfrost for asset protection and Alkanes views: sats providers add subfrost{url}"
        ));
    } else if view.protection.is_none() {
        ui::dim("turn on asset protection: sats providers protect on");
    }
    Ok(())
}

fn endpoint_detail(endpoint: &Endpoint) -> String {
    match endpoint.auth {
        provider::AuthKind::None => endpoint.url.clone(),
        provider::AuthKind::ApiKey => format!("{}  api key", endpoint.url),
        provider::AuthKind::Bearer => format!("{}  bearer token", endpoint.url),
    }
}

fn endpoint_json(endpoint: &Endpoint) -> serde_json::Value {
    serde_json::json!({
        "provider": endpoint.provider,
        "url": endpoint.url,
        "auth": endpoint.auth.as_str(),
        "source": endpoint.source.as_str(),
    })
}

fn overview_json(net_name: &str, view: &Overview) -> serde_json::Value {
    serde_json::json!({
        "network": net_name,
        "chain": endpoint_json(&view.chain),
        "asset_protection": view.protection.as_ref().map(endpoint_json),
        "alkanes_views": view.alkanes.as_ref().map(endpoint_json),
    })
}

/// The network's settings as written, display-safe, for a configuration
/// that does not resolve.
fn configured(config: &Config, network: Network) {
    let net = config.net(network);
    let chain = match net.chain {
        Some(choice) => choice.as_str().to_string(),
        None => format!("{} (default)", ChainChoice::default_for(network).as_str()),
    };
    let redact = crate::provider::error::redact_url;
    let rows = [
        ["Chain data".to_string(), chain, String::new()],
        [
            "Asset protection".into(),
            if net.protect_assets { "on" } else { "off" }.into(),
            String::new(),
        ],
        [
            "Subfrost".into(),
            if config.subfrost.is_some() {
                "key saved"
            } else {
                "no key"
            }
            .into(),
            net.subfrost_url
                .as_ref()
                .map(|u| redact(&u.0))
                .unwrap_or_default(),
        ],
        [
            "Esplora".into(),
            if net.esplora.is_some() {
                "set up"
            } else {
                "—"
            }
            .into(),
            net.esplora
                .as_ref()
                .map(|e| redact(&e.url.0))
                .unwrap_or_default(),
        ],
    ];
    table(&rows);
}

fn table(rows: &[[String; 3]]) {
    let mut widths = [0usize; 3];
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row.iter()) {
            *w = (*w).max(cell.chars().count());
        }
    }
    for row in rows {
        let line = row
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect::<Vec<_>>()
            .join("  ");
        println!("{}", line.trim_end());
    }
}
