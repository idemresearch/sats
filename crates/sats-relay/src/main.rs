//! `sats-relay`: run the keyless relay between sats agent and sats human.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

/// A keyless relay: agents file requests over MCP, the human's device
/// fetches them, decides, and posts results back. It holds no wallet and
/// no key, and it can't approve, sign, or broadcast.
#[derive(Parser)]
#[command(name = "sats-relay", version)]
struct Args {
    /// Address to listen on
    #[arg(long, env = "SATS_RELAY_LISTEN", default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    /// Directory for the mailbox database
    #[arg(long, env = "SATS_RELAY_DATA", default_value = "./sats-relay-data")]
    data_dir: PathBuf,
    /// How long a request waits for the human's device (e.g. 7d)
    #[arg(long, env = "SATS_RELAY_FILING_TTL", default_value = "7d")]
    filing_ttl: humantime::Duration,
    /// How long a posted result stays observable (e.g. 30d)
    #[arg(long, env = "SATS_RELAY_RESULT_TTL", default_value = "30d")]
    result_ttl: humantime::Duration,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config = sats_relay::Config {
        listen: args.listen,
        data_dir: args.data_dir,
        filing_ttl: args.filing_ttl.as_secs(),
        result_ttl: args.result_ttl.as_secs(),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start async runtime")?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(config.listen)
            .await
            .with_context(|| format!("cannot listen on {}", config.listen))?;
        let bound = listener.local_addr()?;
        eprintln!("sats-relay: listening on {bound}");
        sats_relay::serve(listener, config).await
    })
}
