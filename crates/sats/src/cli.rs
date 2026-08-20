use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "sats",
    version,
    about = "Bitcoin signing for humans and agents",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Network: mainnet, signet, testnet4, regtest (overrides config)
    #[arg(long, global = true, value_name = "NET")]
    pub network: Option<String>,

    /// Machine-readable JSON output
    #[arg(long, global = true)]
    pub json: bool,

    /// Data directory override
    #[arg(long, global = true, env = "SATS_DIR", hide = true, value_name = "DIR")]
    pub dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create a new wallet
    Init {
        /// Mnemonic length (12 or 24 words)
        #[arg(long, default_value_t = 12, value_parser = clap::value_parser!(u8).range(12..=24))]
        words: u8,
    },
    /// Show the wallet balance
    Balance {
        /// Skip chain sync, show the cached balance
        #[arg(long)]
        offline: bool,
    },
    /// Show a fresh receive address
    Receive,
    /// Build an unsigned transaction plan
    Plan {
        /// Recipient address
        address: String,
        /// Amount in sats (shorthand ok: 10k, 1.5m)
        #[arg(value_parser = crate::amount::parse)]
        amount: u64,
        /// Fee rate in sat/vB (default: estimated for ~2 blocks)
        #[arg(long, value_name = "SAT_VB")]
        fee_rate: Option<u64>,
    },
    /// Send bitcoin: plan, confirm, sign, broadcast
    Send {
        /// Recipient address
        address: String,
        /// Amount in sats (shorthand ok: 10k, 1.5m)
        #[arg(value_parser = crate::amount::parse)]
        amount: u64,
        /// Fee rate in sat/vB (default: estimated for ~2 blocks)
        #[arg(long, value_name = "SAT_VB")]
        fee_rate: Option<u64>,
        /// Skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
    /// Sign a saved plan or a PSBT file
    Sign {
        /// PSBT file to sign (base64 or binary); default: newest unsigned plan
        #[arg(value_name = "FILE")]
        psbt: Option<PathBuf>,
        /// Plan id (default: newest unsigned plan)
        #[arg(long, value_name = "ID", conflicts_with = "psbt")]
        plan: Option<String>,
    },
    /// Broadcast a signed plan or a raw transaction
    Broadcast {
        /// Plan id (default: newest signed plan)
        #[arg(long, value_name = "ID")]
        plan: Option<String>,
        /// Broadcast a raw transaction hex file instead
        #[arg(long, value_name = "FILE", conflicts_with = "plan")]
        tx: Option<PathBuf>,
    },
    /// Grant an agent a spending budget
    Grant {
        /// Agent name (e.g. claude)
        agent: String,
        /// Total budget in sats (amounts + fees draw it down; shorthand ok: 50k)
        #[arg(long, value_name = "SATS", value_parser = crate::amount::parse)]
        budget: u64,
        /// Grant lifetime (e.g. 24h, 7d)
        #[arg(
            long = "for",
            alias = "expires",
            default_value = "24h",
            value_name = "DURATION"
        )]
        duration: String,
        /// Per-transaction amount cap in sats
        #[arg(long, value_name = "SATS", value_parser = crate::amount::parse)]
        max_tx: Option<u64>,
        /// Per-transaction fee cap in sats
        #[arg(long, value_name = "SATS", value_parser = crate::amount::parse)]
        max_fee: Option<u64>,
    },
    /// Revoke an agent's grant
    Revoke {
        /// Agent name
        agent: String,
    },
    /// List active grants
    Grants,
    /// Run an MCP server exposing wallet tools to an agent
    Mcp {
        /// Agent name the server acts as (must hold an active grant)
        #[arg(long, value_name = "NAME")]
        agent: String,
    },
}
