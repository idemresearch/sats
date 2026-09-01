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
    /// Set up the wallet: create a new one, or restore from a mnemonic backup
    Init {
        /// Mnemonic length for a new wallet (12 or 24 words; implies create)
        #[arg(long, value_parser = clap::value_parser!(u8).range(12..=24),
              conflicts_with = "restore")]
        words: Option<u8>,
        /// Restore an existing wallet from its mnemonic backup
        #[arg(long)]
        restore: bool,
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
        /// Transaction id or unique prefix
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
    /// Run and control satsd, the local signing daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Manage agent spending grants and the agent-facing MCP server
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Alkanes contract tools (experimental, signet-first)
    Alkanes {
        #[command(subcommand)]
        command: AlkanesCommand,
    },
}

#[derive(Subcommand)]
pub enum DaemonCommand {
    /// Run the daemon in the foreground (for systemd, launchd, or a shell)
    Run {
        /// Lock the seed after this much inactivity (e.g. 8h, 30m)
        #[arg(long, default_value = "8h", value_name = "DURATION")]
        auto_lock: String,
    },
    /// Start the daemon in the background
    Start {
        /// Idle lock duration (default: installed service setting, otherwise 8h)
        #[arg(long, value_name = "DURATION")]
        auto_lock: Option<String>,
    },
    /// Install and start a locked per-user macOS service (opt-in)
    Install {
        /// Lock the seed after this much inactivity (e.g. 8h, 30m)
        #[arg(long, default_value = "8h", value_name = "DURATION")]
        auto_lock: String,
    },
    /// Stop and remove the matching macOS service, preserving wallet data
    Uninstall,
    /// Show whether the daemon is running, and whether it can sign
    Status,
    /// Unseal the wallet into the daemon so agent sends can be signed
    Unlock,
    /// Drop the seed from the daemon's memory, without stopping it
    Lock,
    /// Stop the daemon
    Stop,
}

#[derive(Subcommand)]
pub enum AlkanesCommand {
    /// Fetch a contract's bytecode and show its code hash
    Inspect {
        /// Alkane id (e.g. 2:1)
        #[arg(value_name = "BLOCK:TX")]
        id: String,
    },
    /// Simulate a contract call and show the interpreted result
    Simulate {
        /// Alkane id (e.g. 2:1)
        #[arg(value_name = "BLOCK:TX")]
        id: String,
        /// Calldata words (the first is conventionally the opcode)
        #[arg(value_name = "INPUTS")]
        inputs: Vec<u128>,
    },
    /// Execute a contract call: simulate, confirm, sign, broadcast.
    /// Refuses mainnet in this release
    Execute {
        /// Alkane id (e.g. 2:1)
        #[arg(value_name = "BLOCK:TX")]
        id: String,
        /// Calldata words (the first is conventionally the opcode)
        #[arg(value_name = "INPUTS")]
        inputs: Vec<u128>,
        /// Fee rate in sat/vB (default: estimated for configured target)
        #[arg(long, value_name = "SAT_VB")]
        fee_rate: Option<u64>,
        /// Sats carried by the pointer output the call's assets land on
        #[arg(long, value_name = "SATS", default_value_t = 546)]
        postage: u64,
        /// Skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub enum TxCommand {
    /// Broadcast a raw transaction hex file, or a saved transaction by
    /// txid or unique prefix
    Broadcast {
        /// Raw transaction hex file, or a txid/prefix (`sats status` lists them)
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
        #[arg(value_name = "FILE")]
        file: PathBuf,
        /// Write the signed PSBT here instead of staging a broadcast
        #[arg(long, value_name = "FILE")]
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
    /// Fee rate in sat/vB (default: estimated for configured target)
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

#[derive(clap::Args)]
pub struct GrantArgs {
    /// Agent name (e.g. claude)
    pub name: String,
    /// Total budget in sats (amounts + fees draw it down; shorthand ok: 50k)
    #[arg(long, value_name = "SATS", value_parser = crate::amount::parse)]
    pub budget: u64,
    /// Grant lifetime (e.g. 24h, 7d)
    #[arg(
        long = "for",
        alias = "expires",
        default_value = "24h",
        value_name = "DURATION"
    )]
    pub duration: String,
    /// Hard per-transaction amount cap in sats: amounts above it are
    /// refused outright — the only escalation is changing the grant
    #[arg(long, value_name = "SATS", value_parser = crate::amount::parse)]
    pub max_tx: Option<u64>,
    /// Hard per-transaction fee cap in sats
    /// (default: 2% of the budget, at least 1000, never above the budget)
    #[arg(long, value_name = "SATS", value_parser = crate::amount::parse)]
    pub max_fee: Option<u64>,
    /// Authority mode: ask (every send needs a one-time approval) or
    /// observe (read-only)
    #[arg(long, default_value = "ask", value_name = "ask|observe")]
    pub mode: String,
    /// Restrict proposals to these recipients (repeatable). At least one
    /// --to makes the allowlist finite: any other recipient is refused.
    /// Without --to, every recipient may be proposed
    #[arg(long = "to", value_name = "ADDRESS")]
    pub to: Vec<String>,
}

#[derive(Subcommand)]
pub enum AgentCommand {
    /// Grant an agent a spending budget
    Grant(GrantArgs),
    /// Revoke an agent's grant
    Revoke {
        /// Agent name
        name: String,
    },
    /// Set an agent's authority mode (widening requires the password)
    Mode {
        /// Agent name
        name: String,
        /// ask or observe
        #[arg(value_name = "ask|observe")]
        mode: String,
    },
    /// Add a recipient to a grant's allowlist (password required)
    Allow {
        /// Agent name
        name: String,
        /// Recipient address
        address: String,
    },
    /// Remove a recipient from a grant's allowlist (no password)
    Disallow {
        /// Agent name
        name: String,
        /// Recipient address
        address: String,
    },
    /// List active grants
    List,
    /// Approve one asked request exactly once (password required)
    Approve {
        /// Request id or unique prefix (see: sats agent requests)
        id: String,
        /// Approval lifetime (e.g. 1h, 30m)
        #[arg(long = "for", default_value = "1h", value_name = "DURATION")]
        duration: String,
    },
    /// Dismiss a request and revoke its unconsumed approval
    Deny {
        /// Request id or unique prefix
        id: String,
    },
    /// Review agent send requests (asked ones await a human decision)
    Requests {
        /// Include resolved and dismissed requests, not only pending ones
        #[arg(long, conflicts_with = "watch")]
        all: bool,
        /// Stay running and print each request as it newly awaits an
        /// approval — a trusted channel that does not rely on the agent
        /// relaying its own denials. With --json, a JSONL stream
        #[arg(long)]
        watch: bool,
    },
    /// Show the causal log of agent activity, oldest first
    Log {
        /// Maximum events to show (the newest N)
        #[arg(long, default_value_t = 50, value_name = "N")]
        limit: usize,
        /// Only events for one request, by id or unique prefix
        #[arg(long, value_name = "ID")]
        request: Option<String>,
    },
    /// Run an MCP server exposing wallet tools as this agent
    #[cfg(feature = "mcp")]
    Serve {
        /// Agent name the server acts as (must hold an active grant)
        name: String,
    },
}
