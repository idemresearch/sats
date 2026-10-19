//! Which wallet outputs carry alkanes, from the network's indexer.
//!
//! An output carrying an alkane must never fund a plain BTC send: a
//! transaction without a protostone burns the alkanes on its inputs. Every
//! send therefore scans the wallet's outputs and excludes the ones the
//! indexer says carry assets.
//!
//! The scan fails closed. The indexer answers an outpoint it doesn't know
//! (an unconfirmed one, or one newer than its tip) with an empty
//! response, which is indistinguishable from "holds nothing" unless the
//! answer is checked: a response that doesn't echo the outpoint and its
//! value marks the output *unknown*, and an unknown output is excluded
//! like one carrying assets. An unreachable indexer is an error, so
//! nothing is planned. The indexer is still trusted for what it reports;
//! a lying indexer can hide an asset, though not redirect BTC.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use sats_alkanes::id::AlkaneId;
use sats_alkanes::view::AssetBalance;
use sats_core::bitcoin::OutPoint;

use crate::provider::Services;
use crate::walletd::WalletCtx;

/// One wallet output and what it carries.
#[derive(Debug, Clone)]
pub struct ScannedOutput {
    pub outpoint: OutPoint,
    pub value_sat: u64,
    /// Whether the indexer confirmed it knows this output. An unknown
    /// output's balances are empty but unproven.
    pub known: bool,
    /// Empty for an output carrying no alkanes.
    pub balances: Vec<AssetBalance>,
}

impl ScannedOutput {
    /// Proven to carry nothing, so a plain send may spend it.
    pub fn is_clean(&self) -> bool {
        self.known && self.balances.is_empty()
    }
}

/// Every unspent wallet output and what the indexer says it carries.
#[derive(Debug, Clone, Default)]
pub struct WalletAssets {
    pub outputs: Vec<ScannedOutput>,
}

impl WalletAssets {
    /// Outputs a plain send must not spend: carrying alkanes, or unknown.
    pub fn not_clean(&self) -> impl Iterator<Item = &ScannedOutput> {
        self.outputs.iter().filter(|o| !o.is_clean())
    }

    /// Total held per asset, with the indexer's labels.
    pub fn totals(&self) -> BTreeMap<AlkaneId, (u128, String, String)> {
        let mut totals: BTreeMap<AlkaneId, (u128, String, String)> = BTreeMap::new();
        for balance in self.outputs.iter().flat_map(|o| &o.balances) {
            let entry = totals
                .entry(balance.id)
                .or_insert_with(|| (0, balance.name.clone(), balance.symbol.clone()));
            entry.0 = entry.0.saturating_add(balance.amount);
        }
        totals
    }

    pub fn get(&self, outpoint: &OutPoint) -> Option<&ScannedOutput> {
        self.outputs.iter().find(|o| o.outpoint == *outpoint)
    }
}

/// Ask the indexer about every unspent wallet output. The caller has
/// just synced, which also validated the provider's network.
pub fn scan(ctx: &WalletCtx, services: &Services) -> Result<WalletAssets> {
    let mut outputs = Vec::new();
    for utxo in ctx.wallet.list_unspent() {
        let value_sat = utxo.txout.value.to_sat();
        let answer = services.alkanes_outpoint(&utxo.outpoint, value_sat)?;
        let known = answer.outpoint == Some(utxo.outpoint) && answer.value_sat == Some(value_sat);
        if !known && (answer.outpoint.is_some() || answer.value_sat.is_some()) {
            // An answer about some other output, or a different value: not
            // a lag, a disagreement. Refuse rather than guess.
            bail!(
                "the Alkanes indexer answered for {} with a different outpoint or value — \
                 refusing to plan on it",
                utxo.outpoint
            );
        }
        outputs.push(ScannedOutput {
            outpoint: utxo.outpoint,
            value_sat,
            known,
            balances: answer
                .balances
                .into_iter()
                .filter(|b| b.amount > 0)
                .collect(),
        });
    }
    Ok(WalletAssets { outputs })
}

/// Outputs a plain send must not spend, when this network's provider
/// serves Alkanes; `None` when it doesn't, and only the dust heuristic
/// protects assets.
pub fn excluded_for_send(ctx: &WalletCtx, services: &Services) -> Result<Option<Vec<OutPoint>>> {
    if !services.serves_alkanes() {
        return Ok(None);
    }
    let assets = scan(ctx, services)?;
    let excluded: Vec<OutPoint> = assets.not_clean().map(|o| o.outpoint).collect();
    let unknown = assets.outputs.iter().filter(|o| !o.known).count();
    if unknown > 0 {
        log::warn!(
            "{unknown} output{} not yet indexed for Alkanes, excluded until it is",
            if unknown == 1 { "" } else { "s" }
        );
    }
    Ok(Some(excluded))
}
