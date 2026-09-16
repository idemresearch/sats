//! `sats agent serve` — serve wallet tools to an agent over MCP stdio.
//!
//! This process reads the wallet and files requests under the grant its
//! bearer token names. It holds no key material and never prepares,
//! signs, or broadcasts: a human's approval executes a request.
//!
//! The only tokio in the binary lives here (rmcp requires a runtime);
//! every human-facing command stays synchronous.

mod server;

use anyhow::{Context, Result, bail};
use rmcp::ServiceExt;
use sats_core::bitcoin::Network;
use sats_core::fmt::format_sats;
use zeroize::Zeroizing;

use crate::config::network_name;
use crate::store::{Store, now_checked, unix_now};
use crate::ui;
use crate::walletd;

use self::server::TOKEN_ENV;

pub fn run(
    store: &Store,
    network: Network,
    agent: &str,
    providers: Vec<crate::provider::CliProvider>,
) -> Result<()> {
    let net_name = network_name(network);

    // Fail loudly when the MCP client starts the server, not mid-conversation.
    // Order matters: "no grant" names the step a human has not done yet,
    // so it outranks a missing token, which is only meaningful once a
    // grant exists to hold one.
    let grant = store.load_grant(net_name, agent)?.with_context(|| {
        format!("no grant for {agent:?} — run: sats agent grant {agent} --budget <sats>")
    })?;
    // Fail-closed clock: a broken clock refuses to serve rather than
    // treating every grant as unexpired.
    if grant.is_expired(now_checked()?) {
        store.delete_grant(net_name, agent)?;
        bail!("grant for {agent:?} has expired — run: sats agent grant {agent} --budget <sats>");
    }
    let token = Zeroizing::new(std::env::var(TOKEN_ENV).ok().unwrap_or_default());
    if token.is_empty() {
        bail!(
            "no {TOKEN_ENV} in the environment — the token is emitted when you run \
             `sats agent grant {agent} --budget <sats>`; copy one of the local MCP \
             setup commands printed by that grant"
        );
    }
    if !grant.authorizes(&token) {
        bail!(
            "{TOKEN_ENV} does not match the active grant for {agent:?} — a grant issued or \
             replaced later has a different token; re-issue it with: \
             sats agent grant {agent} --budget <sats>"
        );
    }
    // The wallet must exist. Provider selection belongs to chain-reading tools;
    // local filing and observation do not need a usable chain provider.
    walletd::open(store, network)?;
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
        token,
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
