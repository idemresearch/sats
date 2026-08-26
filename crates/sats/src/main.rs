use sats_core::amount;
mod cli;
mod commands;
mod config;
mod daemon;
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
        Command::Init { words, restore } => {
            commands::init::run(&store, config, network, words, restore, &overrides)
        }
        Command::Balance { offline } => {
            commands::balance::run(&store, network, &services(&config)?, offline, json)
        }
        Command::Receive => commands::receive::run(&store, network, json),
        Command::Send(args) => {
            commands::send::run(&store, network, &services(&config)?, &args, json)
        }
        Command::History { offline } => {
            commands::history::run(&store, network, &services(&config)?, offline, json)
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
        Command::Daemon { command } => match command {
            cli::DaemonCommand::Run { auto_lock } => {
                commands::daemon::run(&store, network, &auto_lock)
            }
            cli::DaemonCommand::Start { auto_lock } => {
                commands::daemon::start(&store, network, &auto_lock, json)
            }
            cli::DaemonCommand::Status => commands::daemon::status(&store, network, json),
            cli::DaemonCommand::Unlock => commands::daemon::unlock(&store, network, json),
            cli::DaemonCommand::Lock => commands::daemon::lock(&store, network, json),
            cli::DaemonCommand::Stop => commands::daemon::stop(&store, network, json),
        },
        Command::Agent { command } => match command {
            cli::AgentCommand::Grant {
                name,
                budget,
                duration,
                max_tx,
                max_fee,
                no_max_fee,
            } => commands::grant::run(
                &store, network, &name, budget, &duration, max_tx, max_fee, no_max_fee, json,
            ),
            cli::AgentCommand::Revoke { name } => {
                commands::revoke::run(&store, network, &name, json)
            }
            cli::AgentCommand::List => commands::grants::run(&store, network, json),
            cli::AgentCommand::Approve {
                id,
                max_fee,
                duration,
            } => commands::approve::run(&store, network, &id, max_fee, &duration, json),
            cli::AgentCommand::Deny { id } => commands::deny::run(&store, network, &id, json),
            cli::AgentCommand::Requests { all } => {
                commands::requests::run(&store, network, all, json)
            }
            cli::AgentCommand::Log { limit, request } => {
                commands::agent_log::run(&store, network, limit, request.as_deref(), json)
            }
            #[cfg(feature = "mcp")]
            cli::AgentCommand::Serve { name } => {
                mcp::run(&store, &config, network, &name, overrides.clone())
            }
        },
        Command::Alkanes { command } => match command {
            cli::AlkanesCommand::Inspect { id } => {
                commands::alkanes::inspect(&services(&config)?, &id, json)
            }
            cli::AlkanesCommand::Simulate { id, inputs } => {
                commands::alkanes::simulate(&services(&config)?, &id, &inputs, json)
            }
            cli::AlkanesCommand::Execute {
                id,
                inputs,
                fee_rate,
                postage,
                yes,
            } => commands::alkanes::execute(
                &store,
                network,
                &services(&config)?,
                &id,
                &inputs,
                fee_rate,
                postage,
                yes,
                json,
            ),
        },
    }
}
