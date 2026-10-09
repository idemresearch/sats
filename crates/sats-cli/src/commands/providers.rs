//! `sats providers`: each network's one provider.
//!
//! Every change is a human trust decision written to the config file. A
//! change that points at an endpoint is resolved and checked against the
//! network before it is saved, so a typo or wrong key never becomes the
//! provider the next send uses.

use std::io::IsTerminal;

use anyhow::{Context, Result, bail};
use sats_core::bitcoin::Network;
use sats_wallet::config::{
    ChainChoice, Config, EsploraConfig, RedactedUrl, SubfrostConfig, network_name,
};
use sats_wallet::provider::{self, CliProvider, Endpoint, Source};
use sats_wallet::store::Store;
use zeroize::Zeroizing;

use crate::cli::{AddArgs, ChainArg, ProviderArg};
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
        }
        ProviderArg::Esplora => {
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
            "{} now uses {}",
            network_name(network),
            display_name(choice.as_str())
        ));
        println!();
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
                "{} is back to the default provider: {}",
                network_name(net),
                display_name(ChainChoice::default_for(net).as_str())
            ));
        }
        println!();
    }
    show(&config, None, network, json)
}

/// Resolve the changed configuration, check the chain source against the
/// network, then save. Nothing is written unless the check passes.
fn check_and_save(
    store: &Store,
    config: &Config,
    network: Network,
    credential: bool,
) -> Result<()> {
    let net_name = network_name(network);
    let services = provider::resolve(config, None, network).context("nothing was saved")?;
    let status = ui::StatusLine::start(&format!("checking the {net_name} endpoint…"));
    let checked = services.check_chain();
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

/// Print the network's provider. When it can't be resolved, show the
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
                println!("{net_name}, as configured");
                println!();
                configured(config, network);
                println!();
            }
            return Err(err.into());
        }
    };
    if json {
        println!("{}", overview_json(net_name, &view));
    } else {
        println!("{net_name}  {}", describe(&view));
    }
    Ok(())
}

/// The name people know a provider by.
fn display_name(provider: &str) -> &str {
    match provider {
        "mempool" => "mempool.space",
        "subfrost" => "Subfrost",
        "esplora" => "Esplora",
        other => other,
    }
}

/// `Subfrost · signet.subfrost.io · key saved`, `mempool.space (default)`.
fn describe(endpoint: &Endpoint) -> String {
    let mut parts = vec![display_name(endpoint.provider).to_string()];
    if endpoint.provider != "mempool" {
        let host = endpoint
            .url
            .split_once("://")
            .map_or(endpoint.url.as_str(), |(_, host)| host);
        parts.push(host.to_string());
    }
    match endpoint.auth {
        provider::AuthKind::None => {}
        provider::AuthKind::ApiKey => parts.push("key saved".into()),
        provider::AuthKind::Bearer => parts.push("token saved".into()),
    }
    let note = match endpoint.source {
        Source::Default => " (default)",
        Source::Override => " (--provider, this run only)",
        Source::Config => "",
    };
    format!("{}{note}", parts.join(" · "))
}

fn overview_json(net_name: &str, endpoint: &Endpoint) -> serde_json::Value {
    serde_json::json!({
        "network": net_name,
        "provider": endpoint.provider,
        "url": endpoint.url,
        "auth": endpoint.auth.as_str(),
        "source": endpoint.source.as_str(),
    })
}

/// The network's settings as written, display-safe, for a configuration
/// that does not resolve.
fn configured(config: &Config, network: Network) {
    let net = config.net(network);
    let chain = match net.chain {
        Some(choice) => display_name(choice.as_str()).to_string(),
        None => format!(
            "{} (default)",
            display_name(ChainChoice::default_for(network).as_str())
        ),
    };
    let redact = sats_wallet::provider::error::redact_url;
    let rows = [
        ["Provider".to_string(), chain, String::new()],
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

fn table<const N: usize>(rows: &[[String; N]]) {
    let mut widths = [0usize; N];
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
