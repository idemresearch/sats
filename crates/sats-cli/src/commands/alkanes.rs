//! Alkanes tools: balance, inspect, simulate, execute, send.
//!
//! Views go through the network's provider, which must be Subfrost;
//! `simulate`'s wire format is unverified against a live endpoint, so
//! results are interpreted tolerantly and always displayed verbatim.
//! Execution is human-only: simulate, show, confirm, password-sign. The
//! whole module is experimental
//! (`experimental-alkanes`) and excluded from default builds; execution
//! needs `experimental-alkanes-execute` on top.

#[cfg(feature = "experimental-alkanes-execute")]
use std::collections::BTreeSet;

#[cfg(feature = "experimental-alkanes-execute")]
use anyhow::{Context, bail};
use anyhow::{Result, anyhow};
#[cfg(feature = "experimental-alkanes-execute")]
use sats_alkanes::build::build_execute_plan;
#[cfg(feature = "experimental-alkanes-execute")]
use sats_alkanes::call::AlkaneCall;
#[cfg(feature = "experimental-alkanes-execute")]
use sats_alkanes::delta::SimulationView;
use sats_alkanes::delta::parse_simulation;
use sats_alkanes::id::AlkaneId;
use sats_alkanes::inspect::code_hash;
use sats_core::bitcoin::Network;
#[cfg(feature = "experimental-alkanes-execute")]
use sats_core::bitcoin::{Address, Amount, FeeRate, OutPoint};
use sats_core::fmt::format_sats;
#[cfg(feature = "experimental-alkanes-execute")]
use sats_core::plan::TxOrigin;
#[cfg(feature = "experimental-alkanes-execute")]
use sats_core::{bdk_wallet::KeychainKind, engine};
use sats_wallet::provider::Services;
#[cfg(feature = "experimental-alkanes-execute")]
use sats_wallet::spend;
use sats_wallet::store::Store;
#[cfg(feature = "experimental-alkanes-execute")]
use sats_wallet::store::unix_now;
use sats_wallet::{assets, walletd};

#[cfg(feature = "experimental-alkanes-execute")]
use crate::keys;
use crate::ui;

fn parse_id(id: &str) -> Result<AlkaneId> {
    id.parse::<AlkaneId>().map_err(|e| anyhow!(e))
}

fn label(name: &str, symbol: &str) -> String {
    match (name.is_empty(), symbol.is_empty()) {
        (true, true) => String::new(),
        (false, true) => name.to_string(),
        (true, false) => symbol.to_string(),
        (false, false) => format!("{name} ({symbol})"),
    }
}

/// Sync, then ask the indexer what every wallet output carries.
fn scan_wallet(
    store: &Store,
    network: Network,
    services: &Services,
) -> Result<(walletd::WalletCtx, assets::WalletAssets)> {
    services.require_alkanes()?;
    let mut ctx = walletd::open(store, network)?;
    services
        .sync_wallet(&mut ctx)
        .map_err(|e| anyhow!("{e} — refusing to report on stale state"))?;
    let scanned = assets::scan(&ctx, services)?;
    Ok((ctx, scanned))
}

pub fn balance(store: &Store, network: Network, services: &Services, json: bool) -> Result<()> {
    let (_ctx, scanned) = scan_wallet(store, network, services)?;
    let totals = scanned.totals();
    let unknown: Vec<String> = scanned
        .outputs
        .iter()
        .filter(|o| !o.known)
        .map(|o| o.outpoint.to_string())
        .collect();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "assets": totals.iter().map(|(id, (amount, name, symbol))| serde_json::json!({
                    "id": id.to_string(),
                    "amount": amount.to_string(),
                    "name": name,
                    "symbol": symbol,
                })).collect::<Vec<_>>(),
                "outputs": scanned.outputs.iter().filter(|o| !o.balances.is_empty()).map(|o| serde_json::json!({
                    "outpoint": o.outpoint.to_string(),
                    "value_sat": o.value_sat,
                    "balances": o.balances.iter().map(|b| serde_json::json!({
                        "id": b.id.to_string(),
                        "amount": b.amount.to_string(),
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
                "unindexed_outputs": unknown,
            })
        );
        return Ok(());
    }
    if totals.is_empty() {
        ui::dim("no alkanes in this wallet");
    }
    for (id, (amount, name, symbol)) in &totals {
        println!("  {id:<12} {amount:>24}  {}", label(name, symbol));
    }
    for output in scanned.outputs.iter().filter(|o| !o.balances.is_empty()) {
        let held: Vec<String> = output
            .balances
            .iter()
            .map(|b| format!("{} of {}", b.amount, b.id))
            .collect();
        ui::dim(&format!("{}  {}", output.outpoint, held.join(", ")));
    }
    if !unknown.is_empty() {
        ui::warn(&format!(
            "{} output{} not yet indexed (unconfirmed?): balances unknown, excluded from sends",
            unknown.len(),
            if unknown.len() == 1 { "" } else { "s" }
        ));
    }
    Ok(())
}

pub fn inspect(services: &Services, id: &str, json: bool) -> Result<()> {
    let id = parse_id(id)?;
    let bytecode = services.alkanes_bytecode(id.block, id.tx)?;
    let hash = code_hash(&bytecode);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": id.to_string(),
                "bytecode_bytes": bytecode.len(),
                "code_hash": hash,
            })
        );
    } else {
        ui::kv_rows(&[
            ("Alkane", id.to_string()),
            ("Bytecode", format!("{} bytes", bytecode.len())),
            ("Code hash", hash),
        ]);
        ui::dim("compare the code hash against a build you trust");
    }
    Ok(())
}

pub fn simulate(services: &Services, id: &str, inputs: &[u128], json: bool) -> Result<()> {
    let id = parse_id(id)?;
    let raw = services.alkanes_simulate(id.block, id.tx, inputs)?;
    let view = parse_simulation(&raw);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": id.to_string(),
                "inputs": inputs.iter().map(u128::to_string).collect::<Vec<_>>(),
                "status": view.status,
                "gas_used": view.gas_used,
                "transfers": view.transfers,
                "raw": view.raw,
            })
        );
        return Ok(());
    }
    let mut rows = vec![
        ("Alkane", id.to_string()),
        (
            "Inputs",
            inputs
                .iter()
                .map(u128::to_string)
                .collect::<Vec<_>>()
                .join(" "),
        ),
    ];
    if let Some(status) = view.status {
        rows.push(("Status", status.to_string()));
    }
    if let Some(gas) = view.gas_used {
        rows.push(("Gas", format_sats(gas)));
    }
    ui::kv_rows(&rows);
    if view.transfers.is_empty() {
        ui::dim("no recognizable asset transfers in the result");
    } else {
        for transfer in &view.transfers {
            println!("  → {} of alkane {}", transfer.value, transfer.id);
        }
    }
    println!("{}", serde_json::to_string_pretty(&view.raw)?);
    ui::dim("simulation is advisory: it reflects the endpoint's view, not a guarantee");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[cfg(feature = "experimental-alkanes-execute")]
pub fn execute(
    store: &Store,
    network: Network,
    services: &Services,
    id: &str,
    inputs: &[u128],
    fee_rate: Option<u64>,
    postage: u64,
    yes: bool,
    json: bool,
) -> Result<()> {
    let target = parse_id(id)?;
    let call = AlkaneCall {
        target,
        inputs: inputs.to_vec(),
    };

    // Simulate first, fail closed: a call the endpoint cannot evaluate is
    // not composed. Advisory only — it informs the human, nothing more.
    let view = parse_simulation(&services.alkanes_simulate(target.block, target.tx, inputs)?);

    let mut ctx = walletd::open(store, network)?;
    services
        .sync_wallet(&mut ctx)
        .map_err(|e| anyhow!("{e} — refusing to plan on stale state"))?;

    // Exclusions as the shared send preparation — the dust heuristic,
    // with no escape flag at all, and outputs carrying alkanes.
    let utxos: Vec<(OutPoint, Amount)> = ctx
        .wallet
        .list_unspent()
        .map(|u| (u.outpoint, u.txout.value))
        .collect();
    let mut unspendable: BTreeSet<OutPoint> = engine::dust_suspects(utxos.iter().copied())
        .into_iter()
        .collect();
    unspendable.extend(assets::excluded_for_send(&ctx, services)?.unwrap_or_default());
    let unspendable: Vec<OutPoint> = unspendable.into_iter().collect();

    let rate = match fee_rate {
        Some(sat_vb) => {
            let sat_vb = u32::try_from(sat_vb).unwrap_or(u32::MAX).max(1);
            FeeRate::from_sat_per_vb_u32(sat_vb)
        }
        None => services
            .estimate_fee_rate()
            .context("cannot estimate fee — pass --fee-rate")?,
    };
    let pointer = ctx.wallet.reveal_next_address(KeychainKind::External);
    let prepared = build_execute_plan(
        &mut ctx.wallet,
        &call,
        &pointer.address,
        postage,
        rate,
        &unspendable,
        ctx.net_name,
        unix_now(),
    )?;
    // The revealed pointer and change addresses must survive the artifact.
    ctx.persist()?;

    if !json {
        let mut rows = vec![
            ("Execute", target.to_string()),
            (
                "Inputs",
                inputs
                    .iter()
                    .map(u128::to_string)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
        ];
        if let Some(status) = view.status {
            rows.push(("Simulated", format!("status {status}")));
        }
        ui::kv_rows(&rows);
        print_transfers(&view);
        ui::sat_rows(&[
            ("Postage", prepared.amount_sat),
            ("Fee", prepared.fee_sat),
            ("Total", prepared.total_sat()),
        ]);
    }
    if !yes && !ui::confirm("Sign?", true)? {
        ui::dim("aborted");
        return Ok(());
    }

    let excluded_utxos = prepared.excluded_utxos;
    let mut record =
        spend::sign_to_record(prepared, keys::unlock(store)?, network)?.with_origin(TxOrigin {
            surface: "cli".into(),
            agent: None,
            request_id: None,
            intent_digest: None,
        });
    // Persist before any network call, exactly as every send does.
    store.save_transaction(ctx.net_name, &record)?;
    if !json {
        ui::ok("signed");
    }
    match spend::broadcast_record(store, &mut ctx, services, &mut record) {
        Ok(txid) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "txid": txid.to_string(),
                        "target": target.to_string(),
                        "inputs": inputs.iter().map(u128::to_string).collect::<Vec<_>>(),
                        "postage_sat": record.amount_sat,
                        "fee_sat": record.fee_sat,
                        "total_sat": record.total_sat(),
                        "excluded_utxos": excluded_utxos,
                    })
                );
            } else {
                ui::ok(&format!("broadcast  {txid}"));
            }
            Ok(())
        }
        Err(err) => {
            bail!(
                "{err:#} — transaction {} saved, retry: sats tx broadcast {}",
                record.txid,
                record.txid
            );
        }
    }
}

#[cfg(feature = "experimental-alkanes-execute")]
pub struct SendArgs<'a> {
    pub id: &'a str,
    pub amount: u128,
    pub address: &'a str,
    pub fee_rate: Option<u64>,
    pub postage: u64,
    pub yes: bool,
}

/// Send an exact amount of one alkane. The transaction is checked by
/// decoding it, not trusted from the builder: the recipient must end up
/// with exactly the amount, and every other alkane on a wallet output.
#[cfg(feature = "experimental-alkanes-execute")]
pub fn send(
    store: &Store,
    network: Network,
    services: &Services,
    args: &SendArgs,
    json: bool,
) -> Result<()> {
    use sats_alkanes::transfer::{Sheet, build_transfer_plan, summarize};
    use std::str::FromStr;

    let asset = parse_id(args.id)?;
    if args.amount == 0 {
        bail!("amount must be positive");
    }
    let recipient = Address::from_str(args.address)
        .map_err(|e| anyhow!("invalid address: {e}"))?
        .require_network(network)
        .map_err(|_| anyhow!("address is not valid for this network"))?;

    let (mut ctx, scanned) = scan_wallet(store, network, services)?;

    // Asset inputs: indexed outputs holding the asset, largest first,
    // until they cover the amount.
    let mut holders: Vec<(&assets::ScannedOutput, u128)> = scanned
        .outputs
        .iter()
        .filter(|o| o.known)
        .filter_map(|o| {
            let held: u128 = o
                .balances
                .iter()
                .filter(|b| b.id == asset)
                .map(|b| b.amount)
                .sum();
            (held > 0).then_some((o, held))
        })
        .collect();
    holders.sort_by_key(|h| std::cmp::Reverse(h.1));
    let held_total: u128 = holders.iter().map(|(_, held)| held).sum();
    if held_total < args.amount {
        bail!(
            "this wallet holds {held_total} of {asset} in indexed outputs, less than {}",
            args.amount
        );
    }
    let mut selected = Vec::new();
    let mut covered = 0u128;
    for (output, held) in holders {
        if covered >= args.amount {
            break;
        }
        selected.push(output.outpoint);
        covered += held;
    }

    // Everything else that isn't provably clean, and the dust heuristic,
    // stays out of the BTC funding.
    let utxos: Vec<(OutPoint, Amount)> = ctx
        .wallet
        .list_unspent()
        .map(|u| (u.outpoint, u.txout.value))
        .collect();
    let mut unspendable: BTreeSet<OutPoint> = engine::dust_suspects(utxos.iter().copied())
        .into_iter()
        .collect();
    unspendable.extend(scanned.not_clean().map(|o| o.outpoint));
    let unspendable: Vec<OutPoint> = unspendable.into_iter().collect();

    let rate = match args.fee_rate {
        Some(sat_vb) => {
            let sat_vb = u32::try_from(sat_vb).unwrap_or(u32::MAX).max(1);
            FeeRate::from_sat_per_vb_u32(sat_vb)
        }
        None => services
            .estimate_fee_rate()
            .context("cannot estimate fee — pass --fee-rate")?,
    };
    let wallet_address = ctx.wallet.reveal_next_address(KeychainKind::External);
    let prepared = build_transfer_plan(
        &mut ctx.wallet,
        asset,
        args.amount,
        &recipient,
        &wallet_address.address,
        &selected,
        args.postage,
        rate,
        &unspendable,
        ctx.net_name,
        unix_now(),
    )?;
    ctx.persist()?;

    // Check the transaction itself.
    let psbt = prepared.psbt();
    let tx = &psbt.unsigned_tx;
    let mut input_sat = Vec::new();
    let mut input_sheets = Vec::new();
    for (index, txin) in tx.input.iter().enumerate() {
        let utxo = psbt.inputs[index]
            .witness_utxo
            .as_ref()
            .ok_or_else(|| anyhow!("input {index} has no witness utxo"))?;
        input_sat.push(utxo.value.to_sat());
        let scanned_output = scanned
            .get(&txin.previous_output)
            .filter(|o| o.known)
            .ok_or_else(|| anyhow!("input {} was not indexed", txin.previous_output))?;
        let mut sheet = Sheet::new();
        for b in &scanned_output.balances {
            *sheet.entry(b.id).or_default() += b.amount;
        }
        input_sheets.push(sheet);
    }
    let summary = summarize(
        tx,
        &input_sat,
        &input_sheets,
        asset,
        args.amount,
        &recipient.script_pubkey(),
        |vout| {
            ctx.wallet
                .is_mine(tx.output[vout as usize].script_pubkey.clone())
        },
    )?;
    let name = scanned
        .totals()
        .get(&asset)
        .map(|(_, name, symbol)| label(name, symbol))
        .unwrap_or_default();

    // The review precedes the password in every mode; --json moves it to
    // stderr.
    let returned: Vec<String> = summary
        .returned
        .iter()
        .map(|(id, amount)| format!("{amount} of {id}"))
        .collect();
    let rows = vec![
        (
            "Send",
            format!(
                "{} of {asset}{}",
                args.amount,
                if name.is_empty() {
                    String::new()
                } else {
                    format!(" {name}")
                }
            ),
        ),
        ("To", recipient.to_string()),
        (
            "Kept",
            if returned.is_empty() {
                "nothing else".into()
            } else {
                returned.join(", ")
            },
        ),
        (
            "Postage",
            format!("{} sats to the recipient", summary.recipient_sat),
        ),
        ("Fee", format!("{} sats", summary.fee_sat)),
        (
            "Total",
            format!(
                "{} sats leave the wallet",
                summary.external_sat + summary.fee_sat
            ),
        ),
    ];
    if json {
        ui::kv_rows_stderr(&rows);
    } else {
        ui::kv_rows(&rows);
    }
    let confirmed = args.yes
        || if json {
            ui::confirm_stderr("Sign?", true)?
        } else {
            ui::confirm("Sign?", true)?
        };
    if !confirmed {
        ui::dim("aborted");
        return Ok(());
    }

    let mut record =
        spend::sign_to_record(prepared, keys::unlock(store)?, network)?.with_origin(TxOrigin {
            surface: "cli".into(),
            agent: None,
            request_id: None,
            intent_digest: None,
        });
    store.save_transaction(ctx.net_name, &record)?;
    if !json {
        ui::ok("signed");
    }
    match spend::broadcast_record(store, &mut ctx, services, &mut record) {
        Ok(txid) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "txid": txid.to_string(),
                        "asset": asset.to_string(),
                        "amount": args.amount.to_string(),
                        "recipient": recipient.to_string(),
                        "postage_sat": summary.recipient_sat,
                        "fee_sat": summary.fee_sat,
                        "kept": summary.returned.iter().map(|(id, amount)| serde_json::json!({
                            "id": id.to_string(),
                            "amount": amount.to_string(),
                        })).collect::<Vec<_>>(),
                    })
                );
            } else {
                ui::ok(&format!("broadcast  {txid}"));
            }
            Ok(())
        }
        Err(err) => bail!(
            "{err:#} — transaction {} saved, retry: sats tx broadcast {}",
            record.txid,
            record.txid
        ),
    }
}

#[cfg(feature = "experimental-alkanes-execute")]
fn print_transfers(view: &SimulationView) {
    for transfer in &view.transfers {
        println!("  → {} of alkane {}", transfer.value, transfer.id);
    }
}
