//! Edit a grant's standing recipient allowlist.
//!
//! `allow` widens authority and requires the wallet password; `disallow`
//! tightens and never does. Entries are stored in the same normalized
//! spelling the intent digest hashes, and only these commands — never
//! payment history, successful sends, or approvals — change the list: an
//! agent must not be able to launder an address into "known" by getting
//! one payment to it approved.

use anyhow::{Context, Result, bail};
use sats_core::bitcoin::{Address, Network};
use sats_core::event::{AgentEvent, CONTROL_EVENT_ID, EVENT_FORMAT_VERSION, EventKind};

use crate::config::network_name;
use crate::store::{Store, now_checked, unix_now};
use crate::{keys, ui};

pub fn allow(
    store: &Store,
    network: Network,
    agent: &str,
    address: &str,
    json: bool,
) -> Result<()> {
    let recipient = normalize(address, network)?;
    let net_name = network_name(network);
    let grant = load_active(store, net_name, agent)?;
    let Some(list) = &grant.allowed_recipients else {
        // No allowlist means unrestricted: there is nothing to widen, and
        // silently creating a one-entry list would *tighten* the grant.
        emit(
            json,
            agent,
            &recipient,
            "unrestricted",
            "every recipient is already allowed — this grant has no allowlist; \
             re-issue it with --to to restrict recipients",
        );
        return Ok(());
    };
    if list.iter().any(|entry| entry == &recipient) {
        emit(
            json,
            agent,
            &recipient,
            "already_allowed",
            "already on the allowlist",
        );
        return Ok(());
    }

    if !json {
        ui::kv_rows(&[
            ("Agent", agent.to_string()),
            ("Allow", recipient.clone()),
            ("Direction", "widens authority (password required)".into()),
        ]);
    }
    // Widening is a control-plane act: the password is the authorization.
    keys::verify_password(store)?;
    {
        let _lock = store.lock_grants(net_name)?;
        let mut fresh = load_active(store, net_name, agent)?;
        match &mut fresh.allowed_recipients {
            // The grant lost its allowlist concurrently: nothing to add.
            None => return Ok(()),
            Some(list) => {
                if !list.iter().any(|entry| entry == &recipient) {
                    list.push(recipient.clone());
                }
            }
        }
        store.save_grant(net_name, &fresh)?;
    }
    journal(
        store,
        net_name,
        agent,
        EventKind::RecipientAllowed {
            recipient: recipient.clone(),
        },
    );
    emit(
        json,
        agent,
        &recipient,
        "allowed",
        "the agent may now propose sends to it — each still needs your approval",
    );
    Ok(())
}

pub fn disallow(
    store: &Store,
    network: Network,
    agent: &str,
    address: &str,
    json: bool,
) -> Result<()> {
    let recipient = normalize(address, network)?;
    let net_name = network_name(network);
    let grant = load_active(store, net_name, agent)?;
    if grant.allowed_recipients.is_none() {
        // An allowlist cannot represent "everything except one address".
        bail!(
            "grant for {agent:?} has no allowlist, so every recipient is allowed — an \
             allowlist cannot express \"all except {recipient}\"; re-issue the grant with \
             --to to restrict it"
        );
    }

    // Tightening needs no password: reducing authority stays cheap.
    let emptied;
    {
        let _lock = store.lock_grants(net_name)?;
        let mut fresh = load_active(store, net_name, agent)?;
        let Some(list) = &mut fresh.allowed_recipients else {
            bail!("the grant lost its allowlist while this command ran — retry");
        };
        let before = list.len();
        list.retain(|entry| entry != &recipient);
        if list.len() == before {
            emit(
                json,
                agent,
                &recipient,
                "not_listed",
                "was not on the allowlist",
            );
            return Ok(());
        }
        emptied = list.is_empty();
        store.save_grant(net_name, &fresh)?;
    }
    journal(
        store,
        net_name,
        agent,
        EventKind::RecipientDisallowed {
            recipient: recipient.clone(),
        },
    );
    emit(
        json,
        agent,
        &recipient,
        "disallowed",
        "requests to it are now refused",
    );
    if emptied && !json {
        ui::warn(
            "the allowlist is now empty — every request is refused until a recipient is allowed",
        );
    }
    Ok(())
}

/// Parse and network-check at the boundary; store the canonical spelling
/// the intent digest hashes, so comparisons are exact strings.
fn normalize(address: &str, network: Network) -> Result<String> {
    use std::str::FromStr;
    Ok(Address::from_str(address)
        .map_err(|e| anyhow::anyhow!("invalid address: {e}"))?
        .require_network(network)
        .map_err(|_| anyhow::anyhow!("address is not valid for {}", network_name(network)))?
        .to_string())
}

fn load_active(store: &Store, net_name: &str, agent: &str) -> Result<sats_core::authz::Grant> {
    let grant = store
        .load_grant(net_name, agent)?
        .with_context(|| format!("no grant for {agent:?}"))?;
    if grant.is_expired(now_checked()?) {
        bail!("grant for {agent:?} has expired — re-issue it");
    }
    Ok(grant)
}

fn journal(store: &Store, net_name: &str, agent: &str, kind: EventKind) {
    if let Err(err) = store.append_event(
        net_name,
        &AgentEvent {
            format_version: EVENT_FORMAT_VERSION,
            at: unix_now(),
            network: net_name.to_string(),
            agent: agent.to_string(),
            request_id: CONTROL_EVENT_ID.into(),
            intent_digest: CONTROL_EVENT_ID.into(),
            kind,
        },
    ) {
        eprintln!("⚠ event log append failed: {err:#}");
    }
}

fn emit(json: bool, agent: &str, recipient: &str, state: &str, detail: &str) {
    if json {
        println!(
            "{}",
            serde_json::json!({ "agent": agent, "recipient": recipient, "state": state })
        );
    } else {
        ui::ok(&format!("{state:<12} {recipient}"));
        ui::dim(detail);
    }
}
