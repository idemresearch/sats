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
        Command::Send(args) => {
            commands::send::run(&store, network, &services(&config)?, &args, json)
        }
        Command::Status { txid, offline } => commands::status::run(
            &store,
            network,
            &services(&config)?,
            txid.as_deref(),
            offline,
            json,
        ),
        Command::Psbt { command } => match command {
            cli::PsbtCommand::Inspect { file } => commands::psbt::inspect(network, &file, json),
            cli::PsbtCommand::Sign { file, session, out } => commands::psbt::sign(
                &store,
                network,
                file.as_deref(),
                session.as_deref(),
                out.as_deref(),
                json,
            ),
        },
        Command::Tx { command } => match command {
            cli::TxCommand::Broadcast { target } => {
                commands::tx::broadcast(&store, network, &services(&config)?, &target, json)
            }
        },
        Command::Agent { command } => match command {
            cli::AgentCommand::Grant {
                name,
                budget,
                duration,
                max_tx,
                max_fee,
            } => commands::grant::run(
                &store, network, &name, budget, &duration, max_tx, max_fee, json,
            ),
            cli::AgentCommand::Revoke { name } => {
                commands::revoke::run(&store, network, &name, json)
            }
            cli::AgentCommand::List => commands::grants::run(&store, network, json),
            #[cfg(feature = "mcp")]
            cli::AgentCommand::Serve { name } => {
                mcp::run(&store, &config, network, &name, overrides.clone())
            }
        },
    }
}
