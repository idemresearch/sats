use anyhow::{Context, Result, bail};
use sats_core::authz::{GRANT_FORMAT_VERSION, Grant, new_grant_id};
use sats_core::bitcoin::Network;
use sats_core::fmt::format_sats;
use sats_core::token;

use crate::config::network_name;
use crate::store::{Store, unix_now};
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
        ui::warn("this token is shown once and is not stored — copy it now");
        println!();
        println!("  SATS_AGENT_TOKEN={}", *issued.secret);
        println!();
        ui::dim("add to Claude Code:");
        ui::dim(&format!(
            "  claude mcp add sats --env SATS_AGENT_TOKEN={} -- sats agent serve {agent}",
            *issued.secret
        ));
        ui::dim("review asks:         sats agent requests --watch");
        ui::dim("approve one:         sats agent approve <id>");
        ui::dim(&format!("revoke any time:     sats agent revoke {agent}"));
        ui::dim("the token cannot spend on its own: every send waits for your approval");
    }
    Ok(())
}

fn validate_agent_name(agent: &str) -> Result<()> {
    if !sats_core::authz::valid_agent_name(agent) {
        bail!("{}", sats_core::authz::AGENT_NAME_RULE);
    }
    Ok(())
}
