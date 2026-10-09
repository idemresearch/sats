//! Alkanes contract tools: inspect, simulate, execute.
//!
//! Views go through the network's provider, which must be Subfrost;
//! `simulate`'s wire format is unverified against a live endpoint, so
//! results are interpreted tolerantly and always displayed verbatim.
//! Execution is human-only: simulate, show, confirm, password-sign — and
//! refuses mainnet. The whole module is experimental
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
#[cfg(feature = "experimental-alkanes-execute")]
use sats_core::bitcoin::{Amount, FeeRate, Network, OutPoint};
use sats_core::fmt::format_sats;
#[cfg(feature = "experimental-alkanes-execute")]
use sats_core::plan::TxOrigin;
#[cfg(feature = "experimental-alkanes-execute")]
use sats_core::{bdk_wallet::KeychainKind, engine};
use sats_wallet::provider::Services;
#[cfg(feature = "experimental-alkanes-execute")]
use sats_wallet::store::{Store, unix_now};
#[cfg(feature = "experimental-alkanes-execute")]
use sats_wallet::{spend, walletd};

#[cfg(feature = "experimental-alkanes-execute")]
use crate::keys;
use crate::ui;

fn parse_id(id: &str) -> Result<AlkaneId> {
    id.parse::<AlkaneId>().map_err(|e| anyhow!(e))
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
    // Before anything else, including any wallet or provider access: the
    // encoding and dialect are young, and real bitcoin waits.
    if network == Network::Bitcoin {
        bail!("alkanes execute is not enabled on mainnet in this release — dogfood on signet");
    }
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

    // Exclusions exactly as the shared send preparation — the dust
    // heuristic, with no escape flag at all.
    let utxos: Vec<(OutPoint, Amount)> = ctx
        .wallet
        .list_unspent()
        .map(|u| (u.outpoint, u.txout.value))
        .collect();
    let unspendable: BTreeSet<OutPoint> = engine::dust_suspects(utxos.iter().copied())
        .into_iter()
        .collect();
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
fn print_transfers(view: &SimulationView) {
    for transfer in &view.transfers {
        println!("  → {} of alkane {}", transfer.value, transfer.id);
    }
}
