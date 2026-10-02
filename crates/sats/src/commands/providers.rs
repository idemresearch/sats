//! `sats providers`: see which provider serves what, and set one up by
//! logging in with its API key.
//!
//! Login is a human trust decision written to the config file: it names the
//! endpoint, stores the credential, and — only when asked — enables asset
//! guards. It checks resolution and the endpoint's network before saving,
//! so a typo or wrong key never becomes the provider the next send uses.

use std::collections::BTreeSet;
use std::io::IsTerminal;

use anyhow::{Context, Result, bail};
use sats_core::bitcoin::Network;
use zeroize::Zeroizing;

use crate::cli::{LoginArgs, LoginProvider};
use crate::config::{AuthConfig, Config, ProviderConfig, network_name};
use crate::provider::{self, AuthKind, Capability, CliProvider};
use crate::store::Store;
use crate::ui;

/// The capability set `--assets` enables on Subfrost: chain access, both
/// asset guards, and the Alkanes view.
const SUBFROST_ASSETS: [&str; 3] = ["chain", "guard", "alkanes.view"];

pub fn list(
    config: &Config,
    overrides: &[CliProvider],
    network: Network,
    json: bool,
) -> Result<()> {
    let net_name = network_name(network);
    let providers = match provider::describe(config, overrides, network) {
        Ok(providers) => providers,
        Err(err) => {
            // The list is where a broken configuration gets diagnosed: show
            // the entries as written, then fail with the resolution error.
            if !json {
                println!("{net_name} providers (not resolvable)");
                println!();
                configured_table(config, net_name);
                println!();
            }
            return Err(err.into());
        }
    };

    if json {
        let list: Vec<_> = providers
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "driver": p.driver,
                    "url": p.url,
                    "source": p.source.as_str(),
                    "auth": p.auth.as_str(),
                    "serves": p.serves.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({ "network": net_name, "providers": list })
        );
        return Ok(());
    }

    println!("{net_name} providers");
    println!();
    let rows: Vec<[String; 5]> = providers
        .iter()
        .map(|p| {
            [
                p.name.clone(),
                p.driver.to_string(),
                p.url.clone(),
                auth_label(p.auth).to_string(),
                serves_label(&p.serves),
            ]
        })
        .collect();
    table(&["Name", "Driver", "Endpoint", "Auth", "Serves"], &rows);
    println!();

    if !overrides.is_empty() {
        ui::dim("--provider replaces every configured provider for this run");
    }
    if providers.iter().any(|p| p.serves.is_empty()) {
        ui::dim("— : configured, but another provider serves its capabilities");
    }
    if !providers
        .iter()
        .flat_map(|p| &p.serves)
        .any(|c| c.is_guard())
    {
        ui::dim("asset guards: none — only the 546/330-sat postage check protects inscriptions");
    }
    if overrides.is_empty() && !providers.iter().any(|p| p.driver == "subfrost") {
        let url = if provider::subfrost::default_url(network).is_some() {
            ""
        } else {
            " --url URL"
        };
        ui::dim(&format!(
            "add Subfrost (chain data, asset guards): sats providers login subfrost{url}"
        ));
    }
    let elsewhere = config
        .providers
        .values()
        .filter(|p| p.network != net_name)
        .count();
    if elsewhere > 0 {
        ui::dim(&format!(
            "{elsewhere} more configured for other networks — select one with --network"
        ));
    }
    Ok(())
}

pub fn login(
    store: &Store,
    mut config: Config,
    network: Network,
    args: &LoginArgs,
    json: bool,
) -> Result<()> {
    let net_name = network_name(network);
    let driver = match args.kind {
        LoginProvider::Subfrost => "subfrost",
        LoginProvider::Esplora => "esplora",
    };
    if args.assets && args.kind != LoginProvider::Subfrost {
        bail!("--assets is a Subfrost option: esplora serves chain data only");
    }
    let url = match (&args.url, args.kind) {
        (Some(url), _) => url.clone(),
        (None, LoginProvider::Subfrost) => provider::subfrost::default_url(network)
            .with_context(|| {
                format!("Subfrost has no default endpoint for {net_name} — pass --url")
            })?
            .to_string(),
        (None, LoginProvider::Esplora) => bail!("esplora needs an endpoint: pass --url"),
    };
    let name = entry_name(&config, driver, net_name, args.name.as_deref())?;
    let existing = config.providers.get(&name);
    let current = existing.and_then(|e| {
        e.api_key
            .clone()
            .or_else(|| e.auth.as_ref().and_then(|a| a.bearer.clone()))
    });

    let capabilities = if args.assets {
        Some(SUBFROST_ASSETS.map(String::from).to_vec())
    } else if let Some(entry) = existing {
        entry.capabilities.clone()
    } else if args.kind == LoginProvider::Subfrost
        && !json
        && std::io::stdin().is_terminal()
        && ui::confirm(
            "also protect inscription and Alkanes UTXOs with Subfrost?",
            false,
        )?
    {
        Some(SUBFROST_ASSETS.map(String::from).to_vec())
    } else {
        None
    };
    let replaced = config.providers.insert(
        name.clone(),
        ProviderConfig {
            driver: driver.to_string(),
            network: net_name.to_string(),
            url,
            capabilities,
            api_key: None,
            auth: None,
        },
    );
    // Refuse a configuration the next command could not resolve (an
    // ambiguous chain provider, a bad capability) before asking for the
    // key. Resolution never depends on the credential's value.
    let providers = provider::describe(&config, &[], network)
        .with_context(|| format!("{name} was not saved"))?;

    let credential = read_credential(args.kind, current)?.map(|secret| secret.to_string());
    let entry = config.providers.get_mut(&name).expect("inserted above");
    match args.kind {
        LoginProvider::Subfrost => entry.api_key = credential,
        LoginProvider::Esplora => {
            entry.auth = credential.map(|token| AuthConfig {
                bearer: Some(token),
            })
        }
    }

    let status = ui::StatusLine::start(&format!("checking {driver} on {net_name}…"));
    let checked = provider::check_entry(&name, &config.providers[&name], network);
    status.finish();
    checked.with_context(|| {
        format!("{name} was not saved — check the URL and key, then log in again")
    })?;
    config.save(store)?;

    let serves = providers
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.serves.clone())
        .unwrap_or_default();
    let auth = config.providers[&name].api_key.is_some()
        || config.providers[&name]
            .auth
            .as_ref()
            .is_some_and(|a| a.bearer.is_some());
    if json {
        println!(
            "{}",
            serde_json::json!({
                "name": name,
                "driver": driver,
                "network": net_name,
                "url": crate::provider::error::redact_url(&config.providers[&name].url),
                "authenticated": auth,
                "updated": replaced.is_some(),
                "serves": serves.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
            })
        );
        return Ok(());
    }
    ui::ok(&format!(
        "{} {name}  {net_name}",
        if replaced.is_some() {
            "updated"
        } else {
            "logged in to"
        }
    ));
    ui::kv_rows(&[
        (
            "Endpoint",
            crate::provider::error::redact_url(&config.providers[&name].url),
        ),
        (
            "Auth",
            if auth {
                "stored in config.toml"
            } else {
                "none"
            }
            .into(),
        ),
        ("Serves", serves_label(&serves)),
    ]);
    if driver == "subfrost" && !serves.iter().any(|c| c.is_guard()) {
        let named = if name == "subfrost" {
            String::new()
        } else {
            format!(" --name {name}")
        };
        ui::dim(&format!(
            "asset guards are off — to enable them: sats providers login subfrost{named} --assets"
        ));
    }
    Ok(())
}

pub fn logout(store: &Store, mut config: Config, name: &str, json: bool) -> Result<()> {
    let Some(entry) = config.providers.remove(name) else {
        let known: Vec<&str> = config.providers.keys().map(String::as_str).collect();
        if known.is_empty() {
            bail!("no provider named {name:?}: none are configured");
        }
        bail!(
            "no provider named {name:?} (configured: {})",
            known.join(", ")
        );
    };
    config.save(store)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "name": name,
                "driver": entry.driver,
                "network": entry.network,
                "removed": true,
            })
        );
        return Ok(());
    }
    ui::ok(&format!("logged out of {name}  {}", entry.network));
    ui::dim("its endpoint and credential are removed from config.toml");
    Ok(())
}

/// The config entry a login writes. An explicit name must not repurpose an
/// entry for another driver or network. Otherwise reuse the one entry this
/// driver already has on the network (a re-login rotates its key), or take
/// the driver's name, then `<driver>-<network>`.
fn entry_name(
    config: &Config,
    driver: &str,
    net_name: &str,
    requested: Option<&str>,
) -> Result<String> {
    if let Some(name) = requested {
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            bail!("provider names use letters, digits, '-' and '_'");
        }
        if let Some(entry) = config.providers.get(name)
            && (entry.driver != driver || entry.network != net_name)
        {
            bail!(
                "{name:?} is already configured as {} for {} — choose another --name",
                entry.driver,
                entry.network
            );
        }
        return Ok(name.to_string());
    }
    let same: Vec<&str> = config
        .providers
        .iter()
        .filter(|(_, p)| p.driver == driver && p.network == net_name)
        .map(|(name, _)| name.as_str())
        .collect();
    match same.as_slice() {
        [one] => return Ok(one.to_string()),
        [] => {}
        many => bail!(
            "several {driver} providers on {net_name} ({}) — pick one with --name",
            many.join(", ")
        ),
    }
    [driver.to_string(), format!("{driver}-{net_name}")]
        .into_iter()
        .find(|candidate| !config.providers.contains_key(candidate))
        .with_context(|| format!("pick a name for this {driver} provider with --name"))
}

/// Hidden entry on a terminal, one line on stdin otherwise. Never a CLI
/// argument: argv and shell history leak. Empty input keeps the entry's
/// current credential; Subfrost requires one, Esplora may run without.
fn read_credential(
    provider: LoginProvider,
    current: Option<String>,
) -> Result<Option<Zeroizing<String>>> {
    let label = match provider {
        LoginProvider::Subfrost => "Subfrost API key",
        LoginProvider::Esplora => "bearer token",
    };
    let input = if std::io::stdin().is_terminal() {
        let hint = match (&current, provider) {
            (Some(_), _) => " (enter keeps the current one)",
            (None, LoginProvider::Esplora) => " (enter for none)",
            (None, LoginProvider::Subfrost) => "",
        };
        Zeroizing::new(rpassword::prompt_password(format!("{label}{hint}: "))?)
    } else {
        let mut line = Zeroizing::new(String::new());
        std::io::stdin().read_line(&mut line)?;
        line
    };
    let input = input.trim();
    if !input.is_empty() {
        return Ok(Some(Zeroizing::new(input.to_string())));
    }
    match (current, provider) {
        (Some(current), _) => Ok(Some(Zeroizing::new(current))),
        (None, LoginProvider::Esplora) => Ok(None),
        (None, LoginProvider::Subfrost) => {
            bail!("no {label}: enter it at the prompt, or pipe it on stdin")
        }
    }
}

/// Entries as written, for when resolution fails: no serving information,
/// and the same redaction as everywhere else.
fn configured_table(config: &Config, net_name: &str) {
    let rows: Vec<[String; 5]> = config
        .providers
        .iter()
        .filter(|(_, p)| p.network == net_name)
        .map(|(name, p)| {
            let auth = if p.api_key.is_some() {
                AuthKind::ApiKey
            } else if p.auth.as_ref().is_some_and(|a| a.bearer.is_some()) {
                AuthKind::Bearer
            } else {
                AuthKind::None
            };
            [
                name.clone(),
                p.driver.clone(),
                crate::provider::error::redact_url(&p.url),
                auth_label(auth).to_string(),
                p.capabilities
                    .as_ref()
                    .map(|c| c.join(", "))
                    .unwrap_or_else(|| "default".into()),
            ]
        })
        .collect();
    if rows.is_empty() {
        ui::dim("no providers configured for this network");
    } else {
        table(
            &["Name", "Driver", "Endpoint", "Auth", "Capabilities"],
            &rows,
        );
    }
}

fn auth_label(auth: AuthKind) -> &'static str {
    match auth {
        AuthKind::None => "—",
        AuthKind::ApiKey => "api key",
        AuthKind::Bearer => "bearer",
    }
}

/// Compact capability list: the three chain capabilities together read
/// as `chain`.
fn serves_label(serves: &[Capability]) -> String {
    if serves.is_empty() {
        return "—".into();
    }
    let set: BTreeSet<Capability> = serves.iter().copied().collect();
    let chain = [
        Capability::ChainSync,
        Capability::ChainFees,
        Capability::ChainBroadcast,
    ];
    let grouped = chain.iter().all(|c| set.contains(c));
    let mut parts = Vec::new();
    if grouped {
        parts.push("chain");
    }
    parts.extend(
        set.iter()
            .filter(|c| !(grouped && chain.contains(c)))
            .map(|c| c.as_str()),
    );
    parts.join(", ")
}

fn table<const N: usize>(header: &[&str; N], rows: &[[String; N]]) {
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row.iter()) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: &[String]| {
        cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    ui::dim(&line(&header.map(String::from)));
    for row in rows {
        println!("{}", line(row));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(driver: &str, network: &str) -> ProviderConfig {
        ProviderConfig {
            driver: driver.into(),
            network: network.into(),
            url: "https://example.com".into(),
            capabilities: None,
            api_key: None,
            auth: None,
        }
    }

    #[test]
    fn login_names_reuse_rotate_or_avoid_collisions() {
        let mut config = Config::default();
        assert_eq!(
            entry_name(&config, "subfrost", "signet", None).unwrap(),
            "subfrost"
        );

        // A re-login on the same network rotates the existing entry,
        // whatever it is called.
        config
            .providers
            .insert("assets".into(), entry("subfrost", "signet"));
        assert_eq!(
            entry_name(&config, "subfrost", "signet", None).unwrap(),
            "assets"
        );

        // Another network's entry keeps its name; the new one is suffixed.
        config
            .providers
            .insert("subfrost".into(), entry("subfrost", "mainnet"));
        assert_eq!(
            entry_name(&config, "subfrost", "testnet4", None).unwrap(),
            "subfrost-testnet4"
        );

        // Two candidates on one network: the human picks.
        config
            .providers
            .insert("more".into(), entry("subfrost", "signet"));
        let err = entry_name(&config, "subfrost", "signet", None).unwrap_err();
        assert!(err.to_string().contains("--name"));
    }

    #[test]
    fn explicit_login_names_never_repurpose_another_entry() {
        let mut config = Config::default();
        config
            .providers
            .insert("chain".into(), entry("esplora", "signet"));
        for (driver, network) in [("subfrost", "signet"), ("esplora", "mainnet")] {
            assert!(entry_name(&config, driver, network, Some("chain")).is_err());
        }
        assert_eq!(
            entry_name(&config, "esplora", "signet", Some("chain")).unwrap(),
            "chain"
        );
        for bad in ["", "a b", "x.y", "cli:subfrost"] {
            assert!(entry_name(&config, "subfrost", "signet", Some(bad)).is_err());
        }
    }

    #[test]
    fn serves_label_groups_chain_capabilities() {
        assert_eq!(serves_label(&[]), "—");
        assert_eq!(
            serves_label(&[
                Capability::ChainSync,
                Capability::ChainFees,
                Capability::ChainBroadcast,
                Capability::GuardOrd,
                Capability::AlkanesView,
            ]),
            "chain, guard.ord, alkanes.view"
        );
        assert_eq!(
            serves_label(&[Capability::ChainSync, Capability::GuardAlkanes]),
            "chain.sync, guard.alkanes"
        );
    }
}
