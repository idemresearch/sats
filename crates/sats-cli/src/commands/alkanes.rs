//! Human CLI rendering and authorization for Alkanes workflows.
//! Wallet/provider work lives in sats-wallet::alkanes; pure protocol logic
//! lives in sats-alkanes. MCP has no Alkanes execution capability.

use anyhow::bail;
use anyhow::{Result, anyhow};
use sats_alkanes::call::AlkaneCall;
use sats_alkanes::id::AlkaneId;
use sats_core::bitcoin::Network;
use sats_core::fmt::format_sats;
use sats_wallet::alkanes;
use sats_wallet::alkanes::human::{self, OperationReview, StagedOperation};
use sats_wallet::provider::Services;
use sats_wallet::store::Store;

use crate::keys;
use crate::ui;

fn parse_id(id: &str) -> Result<AlkaneId> {
    id.parse::<AlkaneId>().map_err(|e| anyhow!(e))
}

pub fn frbtc_info(services: &Services, json: bool) -> Result<()> {
    let info = alkanes::wrapping::info(services)?;
    if json {
        println!(
            "{}",
            serde_json::json!({"network": sats_wallet::config::network_name(services.network()), "id":"32:0", "code_hash":info.code_hash, "signer_address":info.signer_address})
        );
    } else {
        ui::kv_rows(&[
            (
                "Network",
                sats_wallet::config::network_name(services.network()).into(),
            ),
            ("Contract", "32:0".into()),
            ("Code hash", info.code_hash),
            ("Signer", info.signer_address),
        ]);
        ui::dim("provider claims: compare with a trusted reference before approving an frBTC wrap");
    }
    Ok(())
}

fn label(name: &str, symbol: &str) -> String {
    match (name.is_empty(), symbol.is_empty()) {
        (true, true) => String::new(),
        (false, true) => name.to_string(),
        (true, false) => symbol.to_string(),
        (false, false) => format!("{name} ({symbol})"),
    }
}

pub fn balance(store: &Store, network: Network, services: &Services, json: bool) -> Result<()> {
    let scanned = alkanes::balance(store, network, services)?;
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
    let result = alkanes::inspect(services, id)?;
    let hash = result.code_hash;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": id.to_string(),
                "bytecode_bytes": result.bytecode_bytes,
                "code_hash": hash,
            })
        );
    } else {
        ui::kv_rows(&[
            ("Alkane", id.to_string()),
            ("Bytecode", format!("{} bytes", result.bytecode_bytes)),
            ("Code hash", hash),
        ]);
        ui::dim("compare the code hash against a build you trust");
    }
    Ok(())
}

pub fn simulate(services: &Services, id: &str, inputs: &[u128], json: bool) -> Result<()> {
    let id = parse_id(id)?;
    let view = alkanes::simulate(
        services,
        &AlkaneCall {
            target: id,
            inputs: inputs.to_vec(),
        },
    )?;
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
    let args = human::ExecuteRequest {
        call: AlkaneCall {
            target: parse_id(id)?,
            inputs: inputs.to_vec(),
        },
        fee_rate,
        postage_sat: postage,
    };
    let staged = human::stage_execute(store, network, services, &args)?;
    let OperationReview::Execute(review) = staged.review().clone() else {
        unreachable!()
    };
    let Some(outcome) = authorize_and_commit(store, staged, yes, json)? else {
        return Ok(());
    };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "txid": outcome.transaction.txid,
                "target": review.call.target.to_string(),
                "inputs": review.call.inputs.iter().map(u128::to_string).collect::<Vec<_>>(),
                "postage_sat": review.postage_sat,
                "fee_sat": review.fee_sat,
                "total_sat": review.postage_sat + review.fee_sat,
                "excluded_utxos": review.excluded_utxos,
            })
        );
    }
    Ok(())
}

pub struct SendArgs<'a> {
    pub id: &'a str,
    pub amount: u128,
    pub address: &'a str,
    pub fee_rate: Option<u64>,
    pub postage: u64,
    pub yes: bool,
}

pub fn send(
    store: &Store,
    network: Network,
    services: &Services,
    args: &SendArgs,
    json: bool,
) -> Result<()> {
    let request = human::TransferRequest {
        asset: parse_id(args.id)?,
        amount: args.amount,
        address: args.address.to_string(),
        fee_rate: args.fee_rate,
        postage_sat: args.postage,
    };
    let staged = human::stage_transfer(store, network, services, &request)?;
    let OperationReview::Transfer(review) = staged.review().clone() else {
        unreachable!()
    };
    let Some(outcome) = authorize_and_commit(store, staged, args.yes, json)? else {
        return Ok(());
    };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "txid": outcome.transaction.txid,
                "asset": review.asset.to_string(),
                "amount": review.amount.to_string(),
                "recipient": review.recipient,
                "postage_sat": review.summary.recipient_sat,
                "fee_sat": review.summary.fee_sat,
                "kept": review.summary.returned.iter().map(|(id, amount)|
                    serde_json::json!({"id": id.to_string(), "amount": amount.to_string()})).collect::<Vec<_>>(),
            })
        );
    }
    Ok(())
}

fn authorize_and_commit(
    store: &Store,
    staged: StagedOperation<'_>,
    yes: bool,
    json: bool,
) -> Result<Option<human::ExecutionOutcome>> {
    let mut rows = vec![
        ("Wallet", staged.wallet_path().display().to_string()),
        (
            "Network",
            sats_wallet::config::network_name(staged.network()).to_string(),
        ),
    ];
    match staged.review() {
        OperationReview::Execute(review) => {
            let predicted = review
                .simulation
                .transfers
                .iter()
                .map(|t| format!("{} of {}", t.value, t.id))
                .collect::<Vec<_>>();
            rows.extend([
                ("Execute", review.call.target.to_string()),
                (
                    "Inputs",
                    review
                        .call
                        .inputs
                        .iter()
                        .map(u128::to_string)
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                (
                    "Simulation",
                    "provider reports status 0 (advisory; effects not guaranteed)".into(),
                ),
                (
                    "Predicted",
                    if predicted.is_empty() {
                        "no recognized asset transfers; effects unknown".into()
                    } else {
                        predicted.join(", ")
                    },
                ),
                ("Return to", review.pointer_address.clone()),
                (
                    "Postage",
                    format!("{} sats retained in the wallet", review.postage_sat),
                ),
                ("Fee", format!("{} sats", review.fee_sat)),
                (
                    "Funding",
                    format!(
                        "{} sats (postage + fee)",
                        review.postage_sat + review.fee_sat
                    ),
                ),
                (
                    "Total",
                    format!(
                        "{} sats leave the wallet",
                        review.external_sat + review.fee_sat
                    ),
                ),
            ]);
        }
        OperationReview::Transfer(review) => {
            let name = label(&review.name, &review.symbol);
            let returned = review
                .summary
                .returned
                .iter()
                .map(|(id, amount)| format!("{amount} of {id}"))
                .collect::<Vec<_>>();
            rows.extend([
                (
                    "Send",
                    format!(
                        "{} of {}{}",
                        review.amount,
                        review.asset,
                        if name.is_empty() {
                            String::new()
                        } else {
                            format!(" {name}")
                        }
                    ),
                ),
                ("To", review.recipient.clone()),
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
                    format!("{} sats to the recipient", review.summary.recipient_sat),
                ),
                ("Fee", format!("{} sats", review.summary.fee_sat)),
                (
                    "Total",
                    format!(
                        "{} sats leave the wallet",
                        review.summary.external_sat + review.summary.fee_sat
                    ),
                ),
            ]);
        }
    }
    if json {
        ui::kv_rows_stderr(&rows);
    } else {
        ui::kv_rows(&rows);
    }
    let confirmed = yes
        || if json {
            ui::confirm_stderr("Sign?", true)?
        } else {
            ui::confirm("Sign?", true)?
        };
    if !confirmed {
        if json {
            eprintln!("aborted");
        } else {
            ui::dim("aborted");
        }
        return Ok(None);
    }
    let network = staged.network();
    let outcome = staged.commit("cli", || {
        Ok(sats_core::signer::LocalSigner::new(
            keys::unlock(store)?,
            network,
        )?)
    })?;
    if let Some(err) = &outcome.broadcast_error {
        let txid = &outcome.transaction.txid;
        bail!("{err:#} — transaction {txid} saved, retry: sats tx broadcast {txid}");
    }
    if !json {
        ui::ok(&format!("broadcast  {}", outcome.transaction.txid));
    }
    Ok(Some(outcome))
}
