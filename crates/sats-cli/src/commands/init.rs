use std::io::{IsTerminal, Write};

use anyhow::{Context, Result, bail};
use sats_core::bitcoin::Network;
use sats_core::seed::MnemonicProblem;
use sats_core::{seal, seed};
use zeroize::Zeroizing;

use crate::config::{Config, network_name};
use crate::provider::CliProvider;
use crate::store::{AAD_SEED, Store};
use crate::{password, ui, walletd};

pub fn run(
    store: &Store,
    config: Config,
    network: Network,
    words: Option<u8>,
    restore: bool,
    overrides: Option<&CliProvider>,
) -> Result<()> {
    let net_name = network_name(network);

    if store.seed_exists() {
        if restore {
            bail!(
                "a seed already exists on this machine ({}) — restore needs an empty data \
                 directory; to add {net_name} to the existing wallet, run `sats init`",
                store.seed_path().display()
            );
        }
        return extend_network(store, network);
    }

    let restore = restore || (words.is_none() && choose_restore()?);
    if restore {
        restore_wallet(store, config, network, overrides)
    } else {
        create_wallet(store, config, network, words.unwrap_or(12))
    }
}

/// The seed is network-independent: extend it to a new network instead of
/// refusing outright.
fn extend_network(store: &Store, network: Network) -> Result<()> {
    let net_name = network_name(network);
    if store.wallet_db_path(net_name).exists() {
        bail!(
            "wallet already exists ({})\n\
             Use `sats balance` or `sats receive` to use this wallet.\n\
             For a separate test wallet, set SATS_DIR to an empty directory and run `sats init`.",
            store.seed_path().display()
        );
    }
    let pw = password::get(false)?;
    let blob = store.read_seed()?;
    let mnemonic_bytes = seal::open(&blob, pw.as_bytes(), AAD_SEED)?;
    let mnemonic =
        seed::parse_mnemonic(std::str::from_utf8(&mnemonic_bytes).context("corrupt seed")?)?;
    let (ext, int) = seed::public_descriptors(&mnemonic, network)?;
    walletd::create(store, network, ext, int)?;
    ui::ok(&format!("wallet extended to {net_name}"));
    Ok(())
}

/// Bare `sats init` on a terminal offers both paths, so someone holding a
/// backup cannot accidentally create a fresh wallet. Non-interactive bare
/// init keeps the historical behavior: create.
fn choose_restore() -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    println!("Set up your wallet:");
    print!("  create a new wallet, or restore from a mnemonic backup? [C/r] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_lowercase().as_str(),
        "r" | "restore"
    ))
}

fn create_wallet(store: &Store, mut config: Config, network: Network, words: u8) -> Result<()> {
    let net_name = network_name(network);
    let mnemonic = seed::generate_mnemonic(words)?;
    let pw = password::get(true)?;
    let blob = seal::seal(mnemonic.to_string().as_bytes(), pw.as_bytes(), AAD_SEED)?;
    store.write_seed(&blob)?;

    let (ext, int) = seed::public_descriptors(&mnemonic, network)?;
    walletd::create(store, network, ext, int)?;

    config.network = net_name.to_string();
    config.save(store)?;

    println!();
    let phrase = mnemonic.to_string();
    let word_list: Vec<&str> = phrase.split(' ').collect();
    for chunk in word_list.chunks(6) {
        println!("  {}", chunk.join(" "));
    }
    println!();
    ui::warn(&format!(
        "write these {words} words down — they will not be shown again"
    ));
    println!();
    ui::ok(&format!("wallet created  {net_name}"));
    Ok(())
}

fn restore_wallet(
    store: &Store,
    mut config: Config,
    network: Network,
    overrides: Option<&CliProvider>,
) -> Result<()> {
    let net_name = network_name(network);
    ui::dim(&format!(
        "Restoring onto {net_name}. A different network is selected with --network."
    ));

    if network == Network::Bitcoin {
        confirm_mainnet_restore()?;
    }

    println!("Enter your backup phrase: 12 or 24 words separated by spaces.");
    ui::dim(
        "Input stays hidden. The phrase is checked before anything is stored — a typo cannot cause harm.",
    );
    let phrase = read_mnemonic()?;
    let mnemonic = match seed::parse_mnemonic(&phrase) {
        Ok(m) => m,
        Err(err) => return Err(mnemonic_guidance(&err)),
    };
    ui::ok("phrase is valid");

    let pw = password::get(true)?;
    let blob = seal::seal(mnemonic.to_string().as_bytes(), pw.as_bytes(), AAD_SEED)?;
    store.write_seed(&blob)?;

    let (ext, int) = seed::public_descriptors(&mnemonic, network)?;
    walletd::create(store, network, ext, int)?;

    config.network = net_name.to_string();
    config.save(store)?;
    ui::ok(&format!("wallet restored  {net_name}"));

    first_sync(store, &config, network, overrides)
}

/// A wrong word is the likely cause of every parse failure; say so, and say
/// that nothing was stored — the reader is checking on real money.
fn mnemonic_guidance(err: &sats_core::error::SeedError) -> anyhow::Error {
    let hint = match seed::mnemonic_problem(err) {
        Some(MnemonicProblem::UnknownWord(pos)) => format!(
            "word {pos} is not on the BIP-39 word list — likely a typo or hard-to-read \
             handwriting (brave/brove, cloud/could)"
        ),
        Some(MnemonicProblem::WordCount(count)) => format!(
            "counted {count} words — a backup phrase has 12 or 24; check for missing or \
             joined words"
        ),
        Some(MnemonicProblem::Checksum) => "every word is valid but the phrase's checksum \
             fails — one word is wrong or two are swapped; re-check against your backup"
            .to_string(),
        _ => "the phrase could not be read as a BIP-39 mnemonic".to_string(),
    };
    anyhow::anyhow!(
        "{hint}. Nothing was stored, and a typo here cannot affect your funds — try again."
    )
}

/// Restoring on mainnet converts a paper backup into a hot, agent-grantable
/// key. A y/n prompt gets reflex-confirmed; typing the consequence does not.
fn confirm_mainnet_restore() -> Result<()> {
    ui::warn("mainnet restore: this machine will hold keys that can spend real funds");
    ui::warn("agent grants you create here will be able to spend from them");
    println!("If you only want to watch a cold wallet, stop — restore is not that.");
    print!("To continue, type: hot wallet\n> ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if line.trim().to_lowercase() != "hot wallet" {
        bail!("mainnet restore cancelled — nothing was stored");
    }
    Ok(())
}

/// Hidden entry on a terminal. Never a CLI argument (argv and shell history
/// leak), and in release builds never a pipe; debug builds read a piped
/// line so the tests can restore (see `password::TEST_SEAM`).
fn read_mnemonic() -> Result<Zeroizing<String>> {
    if std::io::stdin().is_terminal() {
        Ok(Zeroizing::new(rpassword::prompt_password("mnemonic: ")?))
    } else if !password::TEST_SEAM {
        bail!("no mnemonic: run sats on a terminal to enter it");
    } else {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let phrase = Zeroizing::new(line.trim().to_string());
        if phrase.is_empty() {
            bail!("no mnemonic: pipe the phrase on stdin or run interactively");
        }
        Ok(phrase)
    }
}

/// The restore ceremony is not over until the person sees their balance.
/// Offer the first scan right here; the scariest possible result — zero —
/// must explain itself.
fn first_sync(
    store: &Store,
    config: &Config,
    network: Network,
    overrides: Option<&CliProvider>,
) -> Result<()> {
    let net_name = network_name(network);
    let manual =
        "next: run `sats balance` — the first sync rediscovers your history and can take a minute";
    if !std::io::stdin().is_terminal() {
        println!("{manual}");
        return Ok(());
    }
    if !ui::confirm("sync now to find your balance?", true)? {
        println!("{manual}");
        return Ok(());
    }
    let services = match crate::provider::resolve(config, overrides, network) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("✗ provider unavailable ({err:#})");
            println!("{manual}");
            return Ok(());
        }
    };
    let mut ctx = walletd::open(store, network)?;
    if let Err(err) = services.sync_wallet(&mut ctx) {
        eprintln!("✗ sync failed ({err:#})");
        println!("{manual}");
        return Ok(());
    }
    let balance = ctx.wallet.balance();
    let spendable = (balance.confirmed + balance.trusted_pending).to_sat();
    if spendable == 0 {
        ui::warn(&format!("first scan complete — balance 0 on {net_name}"));
        println!("if you expected funds here, your coins are not gone:");
        println!("  - check the network: this wallet is on {net_name}");
        println!("  - sats sees only BIP-86 taproot addresses (bc1p…/tb1p…);");
        println!("    coins on other address types are safe but not visible here");
    } else {
        ui::sat_rows(&[("Balance", spendable)]);
        ui::ok("your wallet history is back");
    }
    Ok(())
}
