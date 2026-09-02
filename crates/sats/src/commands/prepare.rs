use std::collections::BTreeSet;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use sats_core::bitcoin::{Address, Amount, FeeRate, OutPoint};
use sats_core::engine;
use sats_core::plan::PreparedSpend;

use crate::provider::Services;
use crate::store::unix_now;
use crate::ui;
use crate::walletd::WalletCtx;

/// One preparation request. MCP sends always use the defaults for the
/// safety escapes: agents get no bypass.
pub struct PrepareRequest<'a> {
    pub address: &'a str,
    pub amount: u64,
    pub fee_rate: Option<u64>,
    pub allow_dust: bool,
    pub no_guards: bool,
}

impl<'a> PrepareRequest<'a> {
    pub fn for_agent(address: &'a str, amount: u64) -> Self {
        PrepareRequest {
            address,
            amount,
            fee_rate: None,
            allow_dust: false,
            no_guards: false,
        }
    }
}

/// The shared preparation pipeline for human sends and approved agent
/// requests:
/// validate → sync → protect → estimate → build. Ordered so that a request
/// that can never succeed (bad address) fails before any network IO, and
/// spending never plans on stale chain state — a failed sync is a hard
/// error, as is a configured guard that cannot answer.
pub fn build(
    ctx: &mut WalletCtx,
    services: &Services,
    req: &PrepareRequest,
) -> Result<PreparedSpend> {
    let addr = Address::from_str(req.address)
        .map_err(|e| anyhow!("invalid address: {e}"))?
        .require_network(ctx.network)
        .map_err(|_| anyhow!("address is not valid for {}", ctx.net_name))?;
    services
        .sync_wallet(ctx)
        .map_err(|e| anyhow!("{e} — refusing to plan on stale state"))?;

    // Exclusions: the dust heuristic unions with every configured guard;
    // conservatism stacks. Escapes are per-invocation flags only.
    let utxos: Vec<(OutPoint, Amount)> = ctx
        .wallet
        .list_unspent()
        .map(|u| (u.outpoint, u.txout.value))
        .collect();
    let mut unspendable: BTreeSet<OutPoint> = BTreeSet::new();
    if !req.allow_dust {
        let dust = engine::dust_suspects(utxos.iter().copied());
        if !dust.is_empty() {
            eprintln!(
                "⚠ {} utxo{} excluded (dust heuristic: possible inscriptions — --allow-dust to override)",
                dust.len(),
                if dust.len() == 1 { "" } else { "s" },
            );
        }
        unspendable.extend(dust);
    }
    if !req.no_guards && services.has_guards() {
        let outpoints: Vec<OutPoint> = utxos.iter().map(|(op, _)| *op).collect();
        let report = services.protected_outpoints(&outpoints)?;
        let fresh: Vec<OutPoint> = report
            .protected
            .iter()
            .filter(|op| !unspendable.contains(op))
            .copied()
            .collect();
        if !fresh.is_empty() {
            eprintln!(
                "⚠ {} utxo{} excluded (guard: carrying assets)",
                fresh.len(),
                if fresh.len() == 1 { "" } else { "s" },
            );
        }
        unspendable.extend(fresh);
    }
    let unspendable: Vec<OutPoint> = unspendable.into_iter().collect();

    let rate = match req.fee_rate {
        Some(sat_vb) => {
            let sat_vb = u32::try_from(sat_vb).unwrap_or(u32::MAX).max(1);
            FeeRate::from_sat_per_vb_u32(sat_vb)
        }
        None => services
            .estimate_fee_rate()
            .context("cannot estimate fee — pass --fee-rate")?,
    };
    Ok(engine::build_plan(
        &mut ctx.wallet,
        &addr,
        Amount::from_sat(req.amount),
        rate,
        &unspendable,
        ctx.net_name,
        unix_now(),
    )?)
}

pub fn print_block(plan: &PreparedSpend) {
    ui::sat_rows(&[
        ("Send", plan.amount_sat),
        ("Fee", plan.fee_sat),
        ("Total", plan.total_sat()),
    ]);
}
