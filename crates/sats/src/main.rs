use sats_core::amount;
mod cli;
mod commands;
mod config;
mod keys;
#[cfg(feature = "mcp")]
mod mcp;
mod password;
mod provider;
mod request;
mod spend;
mod store;
mod ui;
mod vault;
mod walletd;

use crate::cli::{Cli, Command};
use crate::config::{Config, parse_network};
use crate::store::Store;

fn main() {
    // First: decide protected mode and sanitize the environment before
    // clap (SATS_DIR) or anything else reads it.
    vault::enter();
    let cli = Cli::parse_with_safe_errors();
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

    // Workflows with local branches resolve only when they reach chain work.
    let overrides = cli.provider;
    let services = |config: &Config| provider::resolve(config, overrides.as_ref(), network);

    match cli.command {
        Command::Init { words, restore } => {
            commands::init::run(&store, config, network, words, restore, overrides.as_ref())
        }
        Command::Balance { offline } => {
            commands::balance::run(&store, network, || Ok(services(&config)?), offline, json)
        }
        Command::Receive => commands::receive::run(&store, network, json),
        Command::Send(args) => {
            commands::send::run(&store, network, &services(&config)?, &args, json)
        }
        Command::History { offline } => {
            commands::history::run(&store, network, || Ok(services(&config)?), offline, json)
        }
        Command::Status { txid, offline } => commands::status::run(
            &store,
            network,
            || Ok(services(&config)?),
            txid.as_deref(),
            offline,
            json,
        ),
        Command::Psbt { command } => match command {
            cli::PsbtCommand::Inspect { file } => commands::psbt::inspect(network, &file, json),
            cli::PsbtCommand::Sign { file, out } => {
                commands::psbt::sign(&store, network, &file, out.as_deref(), json)
            }
        },
        Command::Tx { command } => match command {
            cli::TxCommand::Broadcast { target } => {
                commands::tx::broadcast(&store, network, || Ok(services(&config)?), &target, json)
            }
        },
        Command::Agent { command } => match command {
            cli::AgentCommand::Grant(args) => commands::grant::run(&store, network, &args, json),
            cli::AgentCommand::Revoke { name } => {
                commands::revoke::run(&store, network, &name, json)
            }
            cli::AgentCommand::Mode { name, mode } => {
                commands::mode::run(&store, network, &name, &mode, json)
            }
            cli::AgentCommand::Allow { name, address } => {
                commands::recipients::allow(&store, network, &name, &address, json)
            }
            cli::AgentCommand::Disallow { name, address } => {
                commands::recipients::disallow(&store, network, &name, &address, json)
            }
            cli::AgentCommand::List => commands::grants::run(&store, network, json),
            cli::AgentCommand::Approve { id, yes } => commands::approve::run(
                &store,
                network,
                || Ok(services(&config)?),
                id.as_deref(),
                yes,
                json,
            ),
            cli::AgentCommand::Dismiss { id } => commands::dismiss::run(&store, network, &id, json),
            cli::AgentCommand::Requests { all, watch } => {
                commands::requests::run(&store, network, all, watch, json)
            }
            cli::AgentCommand::Log { limit, request } => {
                commands::agent_log::run(&store, network, limit, request.as_deref(), json)
            }
            #[cfg(feature = "mcp")]
            cli::AgentCommand::Serve { name } => {
                mcp::run(&store, network, &name, overrides.clone())
            }
        },
        Command::Providers { command } => match command {
            None | Some(cli::ProvidersCommand::List) => {
                commands::providers::list(&config, overrides.as_ref(), network, json)
            }
            Some(cli::ProvidersCommand::Add(args)) => {
                commands::providers::add(&store, config, network, &args, json)
            }
            Some(cli::ProvidersCommand::Use { source }) => {
                commands::providers::use_chain(&store, config, network, source, json)
            }
            Some(cli::ProvidersCommand::Remove { kind }) => {
                commands::providers::remove(&store, config, network, kind, json)
            }
        },
        #[cfg(feature = "experimental-alkanes")]
        Command::Alkanes { command } => match command {
            cli::AlkanesCommand::Inspect { id } => {
                commands::alkanes::inspect(&services(&config)?, &id, json)
            }
            cli::AlkanesCommand::Simulate { id, inputs } => {
                commands::alkanes::simulate(&services(&config)?, &id, &inputs, json)
            }
            #[cfg(feature = "experimental-alkanes-execute")]
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
