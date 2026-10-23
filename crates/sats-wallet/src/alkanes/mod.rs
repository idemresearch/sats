//! Alkanes workflows shared by native surfaces.
//!
//! Provider I/O and wallet state live here; pure protocol rules live in
//! sats-alkanes. No prompts, rendering, or password access. Human execution
//! is never an MCP or agent capability.

use anyhow::{Result, anyhow, bail};
use sats_alkanes::call::AlkaneCall;
use sats_alkanes::delta::{SimulationView, parse_simulation};
use sats_alkanes::id::AlkaneId;
use sats_alkanes::inspect::code_hash;
use sats_core::bitcoin::Network;

use crate::assets::{self, WalletAssets};
use crate::provider::Services;
use crate::store::Store;
use crate::walletd::{self, WalletCtx};

pub mod human;
pub mod transfer;
pub mod wrapping;

pub struct Inspection {
    pub id: AlkaneId,
    pub bytecode_bytes: usize,
    pub code_hash: String,
}

pub fn inspect(services: &Services, id: AlkaneId) -> Result<Inspection> {
    let bytecode = services.alkanes_bytecode(id.block, id.tx)?;
    Ok(Inspection {
        id,
        bytecode_bytes: bytecode.len(),
        code_hash: code_hash(&bytecode),
    })
}

/// Read-only simulation preserves unknown results for the surface to show.
pub fn simulate(services: &Services, call: &AlkaneCall) -> Result<SimulationView> {
    let raw = services.alkanes_simulate(call.target.block, call.target.tx, &call.inputs)?;
    Ok(parse_simulation(&raw))
}

pub fn balance(store: &Store, network: Network, services: &Services) -> Result<WalletAssets> {
    let (_, scanned) = scan_wallet(store, network, services)?;
    Ok(scanned)
}

fn check_services(network: Network, services: &Services) -> Result<()> {
    sats_core::network::ensure_test_network(network)?;
    if services.network() != network {
        bail!("Alkanes provider network does not match the selected wallet network");
    }
    Ok(())
}

fn scan_wallet(
    store: &Store,
    network: Network,
    services: &Services,
) -> Result<(WalletCtx, WalletAssets)> {
    check_services(network, services)?;
    services.require_alkanes()?;
    let mut ctx = walletd::open(store, network)?;
    services
        .sync_wallet(&mut ctx)
        .map_err(|e| anyhow!("{e} — refusing to report on stale state"))?;
    let scanned = assets::scan(&ctx, services)?;
    Ok((ctx, scanned))
}
