//! Alkanes contract tools: inspect, simulate.
//!
//! Read-only views through the resolved `alkanes.view` provider. The
//! wire dialect is the Subfrost driver's (unverified against a live
//! endpoint — see the in-source dialect note); results are interpreted
//! tolerantly and always displayed verbatim.

use anyhow::{Result, anyhow};
use sats_alkanes::delta::parse_simulation;
use sats_alkanes::id::AlkaneId;
use sats_alkanes::inspect::code_hash;
use sats_core::fmt::format_sats;

use crate::provider::Services;
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
