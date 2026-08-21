//! `sats agent serve` — serve wallet tools to an agent over MCP stdio.
//!
//! The only tokio in the binary lives here (rmcp requires a runtime);
//! every human-facing command stays synchronous.

mod server;

use anyhow::{Context, Result, bail};
use rmcp::ServiceExt;
use sats_core::bitcoin::Network;
use sats_core::fmt::format_sats;

use crate::config::{Config, network_name};
use crate::store::{Store, unix_now};
use crate::ui;
use crate::walletd;

pub fn run(
    store: &Store,
    config: &Config,
    network: Network,
    agent: &str,
    providers: Vec<crate::provider::CliProvider>,
) -> Result<()> {
    let net_name = network_name(network);

    // Fail loudly at startup — `claude mcp add` time — not mid-conversation.
    let grant = store.load_grant(net_name, agent)?.with_context(|| {
        format!("no grant for {agent:?} — run: sats agent grant {agent} --budget <sats>")
    })?;
    if grant.is_expired(unix_now()) {
        store.delete_grant(net_name, agent)?;
        bail!("grant for {agent:?} has expired — run: sats agent grant {agent} --budget <sats>");
    }
    // The wallet must exist, and the provider config must resolve.
    walletd::open(store, network)?;
    crate::provider::resolve(config, &providers, network)?;

    // stdout is the MCP transport; all logging goes to stderr.
    eprintln!(
        "sats agent serve: agent {agent:?} on {net_name} — {} sat remaining, expires in {}",
        format_sats(grant.remaining_sat()),
        ui::human_duration(grant.expires_at.saturating_sub(unix_now())),
    );

    let service = server::SatsMcp::new(
        store.dir_override().map(|p| p.to_path_buf()),
        network,
        agent.to_string(),
        providers,
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start async runtime")?;
    rt.block_on(async move {
        let running = service
            .serve(rmcp::transport::stdio())
            .await
            .context("mcp server failed to start")?;
        running.waiting().await.context("mcp server crashed")?;
        Ok(())
    })
}
