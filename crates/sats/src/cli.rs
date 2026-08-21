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

    /// Provider override for the active network, repeatable:
    /// --provider esplora=URL or --provider subfrost=URL.
    /// Replaces every configured provider for this invocation.
    #[arg(long, global = true, value_name = "KIND=URL",
          value_parser = crate::provider::parse_cli_provider)]
    pub provider: Vec<crate::provider::CliProvider>,

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
    /// Send bitcoin: prepare, confirm, sign, persist, broadcast
    Send(SendArgs),
    /// List the wallet's transactions, newest first
    History {
        /// Skip chain sync; history may be stale
        #[arg(long)]
        offline: bool,
    },
    /// Show pending and broadcast transactions, or one by txid
    Status {
        /// Transaction id, unique prefix, or session id
        #[arg(value_name = "TXID")]
        txid: Option<String>,
        /// Skip chain sync; confirmation state may be stale
        #[arg(long)]
        offline: bool,
    },
    /// Inspect and sign PSBT files (advanced, multi-signer workflows)
    Psbt {
        #[command(subcommand)]
        command: PsbtCommand,
    },
    /// Work with signed transactions (advanced)
    Tx {
        #[command(subcommand)]
        command: TxCommand,
    },
    /// Manage agent spending grants and the agent-facing MCP server
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
}

#[derive(Subcommand)]
pub enum TxCommand {
    /// Broadcast a raw transaction hex file, or a saved transaction by
    /// txid, unique prefix, or session id
    Broadcast {
        /// Raw transaction hex file, or a txid/prefix/id (`sats status` lists them)
        #[arg(value_name = "FILE|TXID")]
        target: String,
    },
}

#[derive(Subcommand)]
pub enum PsbtCommand {
    /// Decode a PSBT file: outputs, fee, and signing state
    Inspect {
        /// PSBT file (base64 text or binary)
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
    /// Sign a PSBT file with the wallet seed
    Sign {
        /// PSBT file (base64 text or binary)
        #[arg(value_name = "FILE", required_unless_present = "session")]
        file: Option<PathBuf>,
        /// Sign a stored PSBT session or pre-refactor plan by id instead
        #[arg(long, value_name = "ID", conflicts_with = "file")]
        session: Option<String>,
        /// Write the signed PSBT here instead of staging a broadcast
        #[arg(long, value_name = "FILE", conflicts_with = "session")]
        out: Option<PathBuf>,
    },
}

#[derive(clap::Args)]
pub struct SendArgs {
    /// Recipient address
    pub address: String,
    /// Amount in sats (shorthand ok: 10k, 1.5m)
    #[arg(value_parser = crate::amount::parse)]
    pub amount: u64,
    /// Fee rate in sat/vB (default: estimated for ~2 blocks)
    #[arg(long, value_name = "SAT_VB")]
    pub fee_rate: Option<u64>,
    /// Spend UTXOs at inscription postage values (546/330 sats)
    #[arg(long)]
    pub allow_dust: bool,
    /// Skip the configured metaprotocol guards, loudly
    #[arg(long)]
    pub no_guards: bool,
    /// Skip the confirmation prompt
    #[arg(short, long, conflicts_with_all = ["dry_run", "export_psbt"])]
    pub yes: bool,
    /// Preview only: prepare and price the send, persist nothing
    #[arg(long, conflicts_with = "export_psbt")]
    pub dry_run: bool,
    /// Write the unsigned PSBT to FILE instead of signing
    #[arg(long, value_name = "FILE")]
    pub export_psbt: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum AgentCommand {
    /// Grant an agent a spending budget
    Grant {
        /// Agent name (e.g. claude)
        name: String,
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
        name: String,
    },
    /// List active grants
    List,
    /// Run an MCP server exposing wallet tools as this agent
    #[cfg(feature = "mcp")]
    Serve {
        /// Agent name the server acts as (must hold an active grant)
        name: String,
    },
}
