//! Human-pinned frBTC wrapping facts and preparation. No signing entry
//! point: agent-originated wraps use request::execute exclusively.

use crate::{
    assets,
    provider::Services,
    store::{Store, unix_now},
    walletd::{self, WalletCtx},
};
use anyhow::{Result, anyhow, bail};
use sats_alkanes::{build::DEFAULT_POSTAGE_SAT, wrap};
use sats_core::authz::SpendRequest;
use sats_core::bitcoin::{Address, Network};
use sats_core::intent::FrbtcContract;
use sats_core::plan::PreparedSpend;
use std::collections::BTreeSet;

/// Public provider claims recorded in the request and reviewed at approval.
/// Never accept a destination from the agent.
pub fn info(services: &Services) -> Result<FrbtcContract> {
    sats_core::network::ensure_test_network(services.network())?;
    services.require_alkanes()?;
    let inspection = super::inspect(services, wrap::FRBTC)?;
    let request = wrap::context_request(&wrap::call(wrap::SIGNER_OPCODE), None, 880_000, 0, 0);
    let raw = services.alkanes_context(&request, true)?;
    let address = wrap::signer_address(&raw, services.network()).map_err(|e| anyhow!(e))?;
    Ok(FrbtcContract {
        signer_address: address.to_string(),
        code_hash: inspection.code_hash,
    })
}

pub(crate) fn check_contract(services: &Services, expected: &FrbtcContract) -> Result<()> {
    if info(services)? != *expected {
        bail!(
            "frBTC code hash or signer differs from the recorded request; dismiss it and review the current frBTC facts before requesting again"
        );
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct WrapReview {
    pub signer_address: String,
    pub code_hash: String,
    pub receive_address: String,
    pub amount_sat: u64,
    pub predicted_frbtc: u128,
    pub postage_sat: u64,
    pub fee_sat: u64,
    pub excluded_utxos: u64,
}

pub(crate) struct PreparedWrap {
    pub ctx: WalletCtx,
    pub prepared: PreparedSpend,
    pub spend: SpendRequest,
    pub review: WrapReview,
}

pub(crate) fn prepare(
    store: &Store,
    network: Network,
    services: &Services,
    contract: &FrbtcContract,
    amount_sat: u64,
) -> Result<PreparedWrap> {
    super::check_services(network, services)?;
    if amount_sat < DEFAULT_POSTAGE_SAT {
        bail!("wrap amount must be at least {DEFAULT_POSTAGE_SAT} sat");
    }
    check_contract(services, contract)?;
    let signer = contract
        .signer_address
        .parse::<Address<_>>()?
        .require_network(network)?;
    let mut ctx = walletd::open(store, network)?;
    services
        .sync_wallet(&mut ctx)
        .map_err(|e| anyhow!("{e} — refusing to plan on stale state"))?;
    let mut excluded: BTreeSet<_> = sats_core::engine::dust_suspects(
        ctx.wallet
            .list_unspent()
            .map(|u| (u.outpoint, u.txout.value)),
    )
    .into_iter()
    .collect();
    excluded.extend(
        assets::excluded_for_send(&ctx, services)?
            .ok_or_else(|| anyhow!("wrapping requires an Alkanes indexer"))?,
    );
    let receiver = ctx
        .wallet
        .reveal_next_address(sats_core::bdk_wallet::KeychainKind::External);
    let prepared = wrap::build_plan(
        &mut ctx.wallet,
        &signer,
        &receiver.address,
        amount_sat,
        services.estimate_fee_rate()?,
        &excluded.into_iter().collect::<Vec<_>>(),
        ctx.net_name,
        unix_now(),
    )?;
    ctx.persist()?;
    let tx = &prepared.psbt().unsigned_tx;
    let fee_sat = wrap::check(
        tx,
        &super::human::input_values(&ctx, prepared.psbt())?,
        &signer,
        amount_sat,
        |vout| {
            ctx.wallet
                .is_mine(tx.output[vout as usize].script_pubkey.clone())
        },
    )
    .map_err(|e| anyhow!("refusing wrap: {e}"))?;
    if fee_sat != prepared.fee_sat {
        bail!("wrap fee differs from prepared metadata");
    }
    let request = wrap::context_request(
        &wrap::call(wrap::WRAP_OPCODE),
        Some(tx),
        u64::from(ctx.wallet.latest_checkpoint().height()) + 1,
        2,
        1,
    );
    let raw = services.alkanes_context(&request, false)?;
    let predicted_frbtc =
        wrap::predicted_frbtc(&raw, amount_sat).map_err(|e| anyhow!("refusing wrap: {e}"))?;
    let review = WrapReview {
        signer_address: signer.to_string(),
        code_hash: contract.code_hash.clone(),
        receive_address: receiver.address.to_string(),
        amount_sat: tx.output[0].value.to_sat(),
        predicted_frbtc,
        postage_sat: DEFAULT_POSTAGE_SAT,
        fee_sat,
        excluded_utxos: prepared.excluded_utxos,
    };
    let spend = SpendRequest {
        amount_sat: review.amount_sat,
        fee_sat,
    };
    Ok(PreparedWrap {
        ctx,
        prepared,
        spend,
        review,
    })
}
