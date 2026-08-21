mod amount;
mod cli;
mod commands;
mod config;
mod keys;
#[cfg(feature = "mcp")]
mod mcp;
mod password;
mod provider;
mod spend;
mod store;
mod ui;
mod walletd;

use clap::Parser;

use crate::cli::{Cli, Command};
use crate::config::{Config, parse_network};
use crate::store::Store;

fn main() {
    let cli = Cli::parse();
    if let Err(err) = run(cli) {
        eprintln!("✗ {err:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let store = Store::open(cli.dir.as_deref())?;
    let config = Config::load(&store)?;
    let net_name = cli
        .network
        .clone()
        .unwrap_or_else(|| config.network.clone());
    let network = parse_network(&net_name)?;
    let json = cli.json;

    // Chain access is resolved lazily per command: purely local commands
    // (receive, sign, grants) never need provider configuration.
    let overrides = cli.provider;
    let services = |config: &Config| provider::resolve(config, &overrides, network);

    match cli.command {
        Command::Init { words } => commands::init::run(&store, config, network, words),
        Command::Balance { offline } => {
            commands::balance::run(&store, network, &services(&config)?, offline, json)
        }
        Command::Receive => commands::receive::run(&store, network, json),
        Command::Plan {
            address,
            amount,
            fee_rate,
            allow_dust,
            no_guards,
        } => commands::plan::run(
            &store,
            network,
            &services(&config)?,
            &commands::plan::PlanRequest {
                address: &address,
                amount,
                fee_rate,
                allow_dust,
                no_guards,
            },
            json,
        ),
        Command::Send {
            address,
            amount,
            fee_rate,
            allow_dust,
            no_guards,
            yes,
        } => commands::send::run(
            &store,
            network,
            &services(&config)?,
            &commands::plan::PlanRequest {
                address: &address,
                amount,
                fee_rate,
                allow_dust,
                no_guards,
            },
            yes,
            json,
        ),
        Command::Sign { psbt, plan } => {
            commands::sign::run(&store, network, plan, psbt.as_deref(), json)
        }
        Command::Broadcast {
            transaction,
            plan,
            tx,
        } => commands::broadcast::run(
            &store,
            network,
            &services(&config)?,
            transaction,
            plan,
            tx.as_deref(),
            json,
        ),
        Command::Grant {
            agent,
            budget,
            duration,
            max_tx,
            max_fee,
        } => commands::grant::run(
            &store, network, &agent, budget, &duration, max_tx, max_fee, json,
        ),
        Command::Revoke { agent } => commands::revoke::run(&store, network, &agent, json),
        Command::Grants => commands::grants::run(&store, network, json),
        #[cfg(feature = "mcp")]
        Command::Mcp { agent } => mcp::run(&store, &config, network, &agent, overrides.clone()),
    }
}
