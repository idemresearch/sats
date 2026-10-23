//! Alkanes transfer preparation and verification shared by native surfaces.
//! No approval or signing entry point: agent transfers use request::execute.

use super::human::{fee_rate, input_values};
use super::{check_services, scan_wallet};
use crate::{
    assets,
    provider::Services,
    store::{Store, unix_now},
    walletd::WalletCtx,
};
use anyhow::{Result, anyhow, bail};
use sats_alkanes::id::AlkaneId;
use sats_alkanes::transfer::{Sheet, TransferSummary, build_transfer_plan, summarize};
use sats_core::bdk_wallet::KeychainKind;
use sats_core::bitcoin::{Address, Amount, Network, OutPoint, Psbt};
use sats_core::{engine, plan::PreparedSpend};
use std::collections::BTreeSet;
use std::str::FromStr;

pub struct TransferRequest {
    pub asset: AlkaneId,
    pub amount: u128,
    pub address: String,
    pub fee_rate: Option<u64>,
    pub postage_sat: u64,
}

#[derive(Debug, Clone)]
pub struct TransferReview {
    pub asset: AlkaneId,
    pub amount: u128,
    pub recipient: String,
    /// Untrusted, informational provider labels.
    pub name: String,
    pub symbol: String,
    pub summary: TransferSummary,
}

pub(crate) struct PreparedTransfer {
    pub ctx: WalletCtx,
    pub prepared: PreparedSpend,
    pub review: TransferReview,
}

pub(crate) fn prepare(
    store: &Store,
    network: Network,
    services: &Services,
    args: &TransferRequest,
) -> Result<PreparedTransfer> {
    check_services(network, services)?;
    let asset = args.asset;
    if args.amount == 0 {
        bail!("amount must be positive");
    }
    let recipient = Address::from_str(&args.address)
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
        .map(|o| {
            let held = o
                .balances
                .iter()
                .filter(|b| b.id == asset)
                .try_fold(0u128, |sum, b| sum.checked_add(b.amount))
                .ok_or_else(|| anyhow!("indexed asset amount overflows"))?;
            Ok((held > 0).then_some((o, held)))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    holders.sort_by_key(|h| std::cmp::Reverse(h.1));
    let held_total = holders
        .iter()
        .try_fold(0u128, |sum, (_, held)| sum.checked_add(*held))
        .ok_or_else(|| anyhow!("indexed asset totals overflow"))?;
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
        covered = covered
            .checked_add(held)
            .ok_or_else(|| anyhow!("selected asset totals overflow"))?;
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

    let rate = fee_rate(services, args.fee_rate)?;
    let wallet_address = ctx.wallet.reveal_next_address(KeychainKind::External);
    let prepared = build_transfer_plan(
        &mut ctx.wallet,
        asset,
        args.amount,
        &recipient,
        &wallet_address.address,
        &selected,
        args.postage_sat,
        rate,
        &unspendable,
        ctx.net_name,
        unix_now(),
    )?;
    ctx.persist()?;

    let summary = verify(
        &ctx,
        prepared.psbt(),
        &scanned,
        asset,
        args.amount,
        &recipient,
    )?;
    if summary.fee_sat != prepared.fee_sat || summary.recipient_sat != prepared.amount_sat {
        bail!("prepared transfer disagrees with the verified transaction");
    }
    let (name, symbol) = scanned
        .totals()
        .get(&asset)
        .map(|(_, name, symbol)| (name.clone(), symbol.clone()))
        .unwrap_or_default();

    let review = TransferReview {
        asset,
        amount: args.amount,
        recipient: recipient.to_string(),
        name,
        symbol,
        summary,
    };
    Ok(PreparedTransfer {
        ctx,
        prepared,
        review,
    })
}

fn verify(
    ctx: &WalletCtx,
    psbt: &Psbt,
    scanned: &assets::WalletAssets,
    asset: AlkaneId,
    amount: u128,
    recipient: &Address,
) -> Result<TransferSummary> {
    let tx = &psbt.unsigned_tx;
    let input_sat = input_values(ctx, psbt)?;
    let mut input_sheets = Vec::new();
    for input in &tx.input {
        let output = scanned
            .get(&input.previous_output)
            .filter(|o| o.known)
            .ok_or_else(|| anyhow!("input {} was not indexed", input.previous_output))?;
        let mut sheet = Sheet::new();
        for balance in &output.balances {
            let slot = sheet.entry(balance.id).or_default();
            *slot = slot
                .checked_add(balance.amount)
                .ok_or_else(|| anyhow!("indexed asset amount overflows"))?;
        }
        input_sheets.push(sheet);
    }
    Ok(summarize(
        tx,
        &input_sat,
        &input_sheets,
        asset,
        amount,
        &recipient.script_pubkey(),
        |vout| {
            ctx.wallet
                .is_mine(tx.output[vout as usize].script_pubkey.clone())
        },
    )?)
}

/// Recheck the reviewed transaction against fresh indexed input balances.
/// No new preparation, no signer, and no changes to the reviewed PSBT.
pub(crate) fn recheck(
    ctx: &WalletCtx,
    services: &Services,
    psbt: &Psbt,
    review: &TransferReview,
) -> Result<()> {
    check_services(ctx.network, services)?;
    let scanned = assets::scan(ctx, services)?;
    let recipient = Address::from_str(&review.recipient)?.require_network(ctx.network)?;
    if verify(ctx, psbt, &scanned, review.asset, review.amount, &recipient)? != review.summary {
        bail!(
            "indexed transfer effects changed after review; authorize a fresh review before signing"
        );
    }
    Ok(())
}
