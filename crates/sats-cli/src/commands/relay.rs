//! `sats relay`: this device's side of a relay. Agents that can't run
//! sats locally file through the relay; this device decides each filing
//! against its own grants, and the human approves here as usual.

use anyhow::Result;
use sats_core::bitcoin::Network;
use sats_wallet::relay::{self, SyncReport};
use sats_wallet::store::Store;

use crate::{mcp, ui};

/// Run a human decision (approve, dismiss), then post every request whose
/// state it changed to the linked relay, so a relayed agent sees the
/// outcome at once. Best effort: the decision stands whatever the relay
/// does, and the next sync posts it anyway.
pub fn after_decision(
    store: &Store,
    network: Network,
    decide: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let Ok(Some(link)) = relay::load(store, network) else {
        return decide();
    };
    let net_name = sats_wallet::config::network_name(network);
    let snapshot = |store: &Store| -> Vec<(String, String, String)> {
        store
            .list_agent_requests(net_name)
            .unwrap_or_default()
            .into_iter()
            .map(|r| (r.id.clone(), r.agent.clone(), r.state.status().to_string()))
            .collect()
    };
    let before = snapshot(store);
    let result = decide();
    let changed: Vec<relay::Outcome> = snapshot(store)
        .into_iter()
        .filter(|entry| !before.contains(entry))
        .map(|(request_id, agent, _)| relay::Outcome::Record { agent, request_id })
        .collect();
    let render = |outcome: &relay::Outcome| mcp::relay_view(store, network, outcome);
    if let Err(error) = relay::post_outcomes(store, network, &link, &changed, &render) {
        ui::warn(&format!(
            "could not update the relay ({error:#}) — sats relay sync will post it"
        ));
    }
    result
}

pub fn link(store: &Store, network: Network, url: &str, new_key: bool, json: bool) -> Result<()> {
    let link = relay::link(store, network, url, new_key)?;
    if json {
        println!(
            "{}",
            serde_json::json!({"url": link.url, "device": link.human()})
        );
    } else {
        ui::ok(&format!("linked  {}", link.url));
        ui::kv_rows(&[("Device", link.human())]);
        println!();
        println!("Next: sats agent grant <name> --relay   (prints the agent's MCP URL)");
        println!("Then: sats relay sync --watch           (brings requests here to approve)");
    }
    Ok(())
}

pub fn sync(
    store: &Store,
    network: Network,
    services: &dyn Fn() -> Result<sats_wallet::provider::Services>,
    watch: bool,
    every: u64,
    json: bool,
) -> Result<()> {
    let link = relay::load(store, network)?.ok_or_else(|| {
        anyhow::anyhow!("no relay linked on this network — run: sats relay link <URL>")
    })?;
    let alkanes = || {
        let services = services()?;
        services.require_alkanes()?;
        Ok(services)
    };
    let render = |outcome: &relay::Outcome| mcp::relay_view(store, network, outcome);
    if !watch {
        let report = relay::sync(store, network, &link, &alkanes, &render)?;
        print_report(store, network, &report, json);
        return Ok(());
    }
    if !json {
        ui::dim(&format!(
            "syncing with {} every {every}s — Ctrl-C to stop",
            link.url
        ));
    }
    loop {
        match relay::sync(store, network, &link, &alkanes, &render) {
            Ok(report) => {
                let quiet = report.created.is_empty()
                    && report.refused.is_empty()
                    && report.deferred.is_empty();
                if json || !quiet {
                    print_report(store, network, &report, json);
                }
            }
            Err(error) => ui::warn(&format!("sync failed: {error:#}")),
        }
        std::thread::sleep(std::time::Duration::from_secs(every.max(1)));
    }
}

fn print_report(store: &Store, network: Network, report: &SyncReport, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "created": report.created,
                "refreshed": report.refreshed,
                "refused": report.refused.iter()
                    .map(|(id, code)| serde_json::json!({"request_id": id, "error_code": code}))
                    .collect::<Vec<_>>(),
                "deferred": report.deferred.iter()
                    .map(|(id, why)| serde_json::json!({"request_id": id, "message": why}))
                    .collect::<Vec<_>>(),
            })
        );
        return;
    }
    let net_name = sats_wallet::config::network_name(network);
    for id in &report.created {
        let described = store
            .find_agent_request(net_name, id)
            .map(|r| {
                format!(
                    "{} asks: {} sat  ({})",
                    r.agent,
                    r.amount_sat,
                    r.state.status()
                )
            })
            .unwrap_or_else(|_| "new request".into());
        ui::ok(&format!("{id}  {described}"));
        println!("    review and approve: sats agent approve {id}");
    }
    for (id, code) in &report.refused {
        ui::warn(&format!("{id}  refused: {code}"));
    }
    for (id, why) in &report.deferred {
        ui::warn(&format!("{id}  will retry: {why}"));
    }
    if report.created.is_empty() && report.refused.is_empty() && report.deferred.is_empty() {
        ui::dim(&format!(
            "nothing new ({} request(s) refreshed)",
            report.refreshed.len()
        ));
    }
}
