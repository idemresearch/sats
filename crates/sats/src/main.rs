mod cli;

use anyhow::bail;
use clap::Parser;

use crate::cli::{Cli, Command};

fn main() {
    let cli = Cli::parse();
    if let Err(err) = run(cli) {
        eprintln!("✗ {err:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        _ => bail!("not implemented yet"),
    }
}
