use anyhow::{Context, Result, bail};
use sats_core::authz::{GRANT_FORMAT_VERSION, Grant};
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
    if budget == 0 {
        bail!("budget must be greater than 0");
    }
    let lifetime = humantime::parse_duration(&args.duration)
        .with_context(|| format!("invalid --for {:?} (try 24h, 7d)", args.duration))?
        .as_secs();
    if lifetime == 0 {
        bail!("--for must be a positive duration");
    }
    // The hard ceiling bounds what may even be asked for; a ceiling below
    // the automatic cap would make the ask band negative-width nonsense.
    if let (Some(max_tx), Some(ask_max_tx)) = (args.max_tx, args.ask_max_tx)
        && ask_max_tx < max_tx
    {
        bail!(
            "--ask-max-tx ({} sat) must be at least --max-tx ({} sat) — amounts up to \
             --max-tx are automatic, up to --ask-max-tx are approvable, above it never",
            format_sats(ask_max_tx),
            format_sats(max_tx),
        );
    }
    // A grant with no fee cap lets one bad fee estimate burn the whole
    // budget as miner fees, so the cap defaults on. Lifting it entirely
    // is `--no-max-fee`, an explicit choice.
    let defaulted_fee = args.max_fee.is_none() && !args.no_max_fee;
    let max_fee = match (args.max_fee, args.no_max_fee) {
        (Some(cap), _) => Some(cap),
        (None, true) => None,
        (None, false) => Some(sats_core::authz::default_max_fee_sat(budget)),
    };
    let net_name = network_name(network);
    let replacing = store.load_grant(net_name, agent)?.is_some();

    if !json {
        let mut rows = vec![
            ("Grant", agent.to_string()),
            ("Budget", format!("{} sat", format_sats(budget))),
        ];
        if let Some(max_tx) = args.max_tx {
            rows.push(("Max tx", format!("{} sat", format_sats(max_tx))));
        }
        if let Some(ask_max_tx) = args.ask_max_tx {
            rows.push((
                "Ask max",
                format!(
                    "{} sat (above this: never approvable)",
                    format_sats(ask_max_tx)
                ),
            ));
        }
        if let Some(max_fee) = max_fee {
            let row = if defaulted_fee {
                format!(
                    "{} sat (default — set with --max-fee, lift with --no-max-fee)",
                    format_sats(max_fee)
                )
            } else {
                format!("{} sat", format_sats(max_fee))
            };
            rows.push(("Max fee", row));
        }
        rows.push(("For", ui::human_duration(lifetime)));
        ui::kv_rows(&rows);
        if network == Network::Bitcoin {
            ui::warn("mainnet grant — this agent will spend real bitcoin");
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
        mode: Default::default(),
        ask_max_tx_sat: args.ask_max_tx,
        allowed_recipients: None,
        suspended: None,
        strikes: Vec::new(),
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
                "budget_sat": grant.budget_sat,
                "max_tx_sat": grant.max_tx_sat,
                "ask_max_tx_sat": grant.ask_max_tx_sat,
                "max_fee_sat": grant.max_fee_sat,
                "expires_at": grant.expires_at,
                "replaced": replacing,
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
        ui::dim(&format!("revoke any time:     sats agent revoke {agent}"));
        ui::dim("the token spends only this budget; it cannot recover the seed");
    }
    Ok(())
}

fn validate_agent_name(agent: &str) -> Result<()> {
    if !sats_core::authz::valid_agent_name(agent) {
        bail!("{}", sats_core::authz::AGENT_NAME_RULE);
    }
    Ok(())
}
