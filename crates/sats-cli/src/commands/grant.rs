use anyhow::{Context, Result, bail};
use sats_core::authz::{GRANT_FORMAT_VERSION, Grant, new_grant_id};
use sats_core::bitcoin::Network;
use sats_core::fmt::format_sats;
use sats_core::token;
use sats_wallet::config::network_name;
use sats_wallet::store::{Store, unix_now};

use crate::{keys, ui};

pub fn run(
    store: &Store,
    network: Network,
    args: &crate::cli::GrantArgs,
    json: bool,
) -> Result<()> {
    let agent = args.name.as_str();
    let budget = args.budget;
    validate_agent_name(agent)?;
    let mode: sats_core::authz::GrantMode =
        args.mode.parse().map_err(|e: String| anyhow::anyhow!(e))?;
    if budget == 0 {
        bail!("budget must be greater than 0");
    }
    let lifetime = humantime::parse_duration(&args.duration)
        .with_context(|| format!("invalid --for {:?} (try 24h, 7d)", args.duration))?
        .as_secs();
    if lifetime == 0 {
        bail!("--for must be a positive duration");
    }
    // Recipients are parsed and network-checked here, at the boundary,
    // and stored in the canonical spelling the intent digest hashes.
    // No --to means no allowlist: every recipient, as before.
    let allowed_recipients = if args.to.is_empty() {
        None
    } else {
        let mut list: Vec<String> = Vec::new();
        for address in &args.to {
            let normalized = std::str::FromStr::from_str(address)
                .map_err(|e| anyhow::anyhow!("invalid --to address: {e}"))
                .and_then(|a: sats_core::bitcoin::Address<_>| {
                    a.require_network(network).map_err(|_| {
                        anyhow::anyhow!(
                            "--to address {address} is not valid for {}",
                            network_name(network)
                        )
                    })
                })?
                .to_string();
            if !list.contains(&normalized) {
                list.push(normalized);
            }
        }
        Some(list)
    };
    // A grant with no fee cap would let one bad fee estimate burn the
    // whole budget as miner fees, so every grant carries one: chosen
    // with --max-fee, defaulted otherwise.
    let defaulted_fee = args.max_fee.is_none();
    let max_fee = args
        .max_fee
        .unwrap_or_else(|| sats_core::authz::default_max_fee_sat(budget));
    let net_name = network_name(network);
    let replacing = store.load_grant(net_name, agent)?.is_some();
    // Resolve the launch context before issuing a token, so a path that
    // cannot be represented in shell instructions does not strand a grant.
    let mcp_command = if json {
        None
    } else {
        Some(mcp_launch_command(store, net_name, agent)?)
    };

    if !json {
        let mut rows = vec![
            ("Grant", agent.to_string()),
            ("Budget", format!("{} sat", format_sats(budget))),
        ];
        rows.push((
            "Mode",
            match mode {
                sats_core::authz::GrantMode::Ask => {
                    "ask — every send needs your approval".to_string()
                }
                sats_core::authz::GrantMode::Observe => "observe — read-only".to_string(),
            },
        ));
        if let Some(list) = &allowed_recipients {
            for recipient in list {
                rows.push(("To", recipient.clone()));
            }
        }
        if let Some(max_tx) = args.max_tx {
            rows.push(("Max tx", format!("{} sat", format_sats(max_tx))));
        }
        let fee_row = if defaulted_fee {
            format!(
                "{} sat (default — choose with --max-fee)",
                format_sats(max_fee)
            )
        } else {
            format!("{} sat", format_sats(max_fee))
        };
        rows.push(("Max fee", fee_row));
        rows.push(("For", ui::human_duration(lifetime)));
        ui::kv_rows(&rows);
        if network == Network::Bitcoin {
            ui::warn("mainnet grant — approvals you issue will spend real bitcoin");
        }
    }

    // The password prompt IS the human authorization. Nothing derived
    // from it goes into the grant: the file that follows holds a policy
    // and a token hash, never key material.
    keys::verify_password(store)?;

    let issued = token::generate()?;
    let now = unix_now();
    let grant = Grant {
        format_version: GRANT_FORMAT_VERSION,
        agent: agent.to_string(),
        network: net_name.to_string(),
        budget_sat: budget,
        spent_sat: 0,
        max_tx_sat: args.max_tx,
        max_fee_sat: max_fee,
        created_at: now,
        expires_at: now.saturating_add(lifetime),
        tx_count: 0,
        token_id: issued.token_id.clone(),
        token_hash: issued.token_hash.clone(),
        mode,
        allowed_recipients,
        // The identity every request filed under this grant binds to:
        // 128 random bits, never reused by a re-issue.
        grant_id: new_grant_id().map_err(|e| anyhow::anyhow!(e))?,
        reservations: Vec::new(),
    };
    // Under the grant lock so an in-flight agent send cannot interleave
    // its budget write with this replacement.
    {
        let _lock = store.lock_grants(net_name)?;
        store.save_grant(net_name, &grant)?;
    }

    if json {
        // The token is emitted once, here, because there is nowhere else
        // it could come from later: only its hash is stored.
        println!(
            "{}",
            serde_json::json!({
                "agent": grant.agent,
                "mode": grant.mode.as_str(),
                "allowed_recipients": grant.allowed_recipients,
                "budget_sat": grant.budget_sat,
                "max_tx_sat": grant.max_tx_sat,
                "max_fee_sat": grant.max_fee_sat,
                "expires_at": grant.expires_at,
                "replaced": replacing,
                "grant_id": grant.grant_id,
                "token_id": grant.token_id,
                "token": &*issued.secret,
            })
        );
    } else {
        println!();
        if replacing {
            ui::ok(&format!(
                "granted  {agent} (previous grant replaced — its old token is now dead)"
            ));
        } else {
            ui::ok(&format!("granted  {agent}"));
        }
        println!();
        println!("Copy one command now; sats cannot recover its embedded token.");
        println!();
        let token_env = shell_quote(&format!("SATS_AGENT_TOKEN={}", *issued.secret));
        let mcp_command = mcp_command.expect("human output has a launch command");
        println!("ChatGPT desktop / Codex:");
        println!("codex mcp add sats --env {token_env} -- {mcp_command}");
        println!();
        println!("Claude Code (current project):");
        println!(
            "claude mcp add --transport stdio --scope local sats --env {token_env} -- {mcp_command}"
        );
        println!();
        if grant.mode == sats_core::authz::GrantMode::Ask {
            println!("Approve: sats agent approve");
        }
        println!("Revoke:  sats agent revoke {agent}");
    }
    Ok(())
}

/// Pin the same configuration and data locations the grant used. An
/// explicit directory takes precedence over SATS_DIR. When the default
/// config and data directories are identical, one explicit --dir is the
/// shortest equivalent command. A split default layout retains the directory
/// resolver's roots and removes a future client's SATS_DIR in the launched
/// process itself.
fn mcp_launch_command(store: &Store, network: &str, agent: &str) -> Result<String> {
    let mut args = Vec::new();
    let config_path = store.config_path();
    let seed_path = store.seed_path();
    let shared_default_dir = match (config_path.parent(), seed_path.parent()) {
        (Some(config_dir), Some(data_dir)) if config_dir == data_dir => Some(data_dir),
        _ => None,
    };
    let pinned_dir = store.dir_override().or(shared_default_dir);

    if pinned_dir.is_none() {
        let dirs = directories::BaseDirs::new().context("cannot determine home directory")?;
        args.extend(["env".into(), "-u".into(), "SATS_DIR".into()]);
        for (name, path) in [
            ("HOME", dirs.home_dir()),
            ("XDG_CONFIG_HOME", dirs.config_dir()),
            ("XDG_DATA_HOME", dirs.data_dir()),
        ] {
            args.push(format!("{name}={}", absolute_shell_path(path)?));
        }
    }
    args.extend(["sats".into(), "--network".into(), network.into()]);
    if let Some(dir) = pinned_dir {
        args.extend(["--dir".into(), absolute_shell_path(dir)?]);
    }
    args.extend(["agent".into(), "serve".into()]);
    if agent.starts_with('-') {
        args.push("--".into());
    }
    args.push(agent.into());
    Ok(args
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" "))
}

fn absolute_shell_path(path: &std::path::Path) -> Result<String> {
    std::path::absolute(path)
        .context("cannot resolve wallet location for MCP instructions")?
        .to_str()
        .map(str::to_owned)
        .context("wallet location is not UTF-8; cannot print lossless MCP shell instructions")
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn validate_agent_name(agent: &str) -> Result<()> {
    if !sats_core::authz::valid_agent_name(agent) {
        bail!("{}", sats_core::authz::AGENT_NAME_RULE);
    }
    Ok(())
}
