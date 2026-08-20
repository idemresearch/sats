use anyhow::{Context, Result, bail};
use sats_core::authz::Grant;
use sats_core::bitcoin::Network;
use sats_core::fmt::format_sats;
use sats_core::seal;

use crate::config::network_name;
use crate::store::{Store, grant_aad, unix_now};
use crate::{keys, ui};

#[allow(clippy::too_many_arguments)]
pub fn run(
    store: &Store,
    network: Network,
    agent: &str,
    budget: u64,
    duration: &str,
    max_tx: Option<u64>,
    max_fee: Option<u64>,
    json: bool,
) -> Result<()> {
    validate_agent_name(agent)?;
    if budget == 0 {
        bail!("budget must be greater than 0");
    }
    let lifetime = humantime::parse_duration(duration)
        .with_context(|| format!("invalid --for {duration:?} (try 24h, 7d)"))?
        .as_secs();
    if lifetime == 0 {
        bail!("--for must be a positive duration");
    }
    let net_name = network_name(network);
    let replacing = store.load_grant(net_name, agent)?.is_some();

    if !json {
        let mut rows = vec![
            ("Grant", agent.to_string()),
            ("Budget", format!("{} sat", format_sats(budget))),
        ];
        if let Some(max_tx) = max_tx {
            rows.push(("Max tx", format!("{} sat", format_sats(max_tx))));
        }
        if let Some(max_fee) = max_fee {
            rows.push(("Max fee", format!("{} sat", format_sats(max_fee))));
        }
        rows.push(("For", ui::human_duration(lifetime)));
        ui::kv_rows(&rows);
        if network == Network::Bitcoin {
            ui::warn("mainnet grant — this agent will spend real bitcoin");
        }
    }

    // The password prompt IS the human authorization.
    let mnemonic = keys::unlock(store)?;

    let grant_key = seal::generate_key_b64()?;
    let key = seal::decode_key_b64(&grant_key)?;
    let wrapped_seed = seal::seal_with_key(
        mnemonic.to_string().as_bytes(),
        &key,
        &grant_aad(net_name, agent),
    )?;
    let now = unix_now();
    let grant = Grant {
        agent: agent.to_string(),
        network: net_name.to_string(),
        budget_sat: budget,
        spent_sat: 0,
        max_tx_sat: max_tx,
        max_fee_sat: max_fee,
        created_at: now,
        expires_at: now.saturating_add(lifetime),
        tx_count: 0,
        wrapped_seed,
        grant_key,
    };
    store.save_grant(net_name, &grant)?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "agent": grant.agent,
                "budget_sat": grant.budget_sat,
                "max_tx_sat": grant.max_tx_sat,
                "max_fee_sat": grant.max_fee_sat,
                "expires_at": grant.expires_at,
                "replaced": replacing,
            })
        );
    } else {
        println!();
        if replacing {
            ui::ok(&format!("granted  {agent} (previous grant replaced)"));
        } else {
            ui::ok(&format!("granted  {agent}"));
        }
        ui::dim(&format!(
            "add to Claude Code:  claude mcp add sats -- sats mcp --agent {agent}"
        ));
        ui::dim(&format!("revoke any time:     sats revoke {agent}"));
    }
    Ok(())
}

fn validate_agent_name(agent: &str) -> Result<()> {
    let ok = !agent.is_empty()
        && agent.len() <= 32
        && agent
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !ok {
        bail!("agent name must be 1-32 chars of a-z, 0-9, - or _");
    }
    Ok(())
}
