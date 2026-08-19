mod cli;
mod commands;
mod config;
mod password;
mod store;
mod ui;
mod walletd;

use anyhow::bail;
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
    let net_name = cli.network.clone().unwrap_or_else(|| config.network.clone());
    let network = parse_network(&net_name)?;

    match cli.command {
        Command::Init { words } => commands::init::run(&store, config, network, words),
        Command::Balance { offline } => commands::balance::run(&store, &config, network, offline, cli.json),
        Command::Receive => commands::receive::run(&store, &config, network, cli.json),
        _ => bail!("not implemented yet"),
    }
}
