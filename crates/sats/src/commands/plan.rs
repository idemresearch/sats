use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use sats_core::bitcoin::{Address, Amount, FeeRate, Network};
use sats_core::engine;
use sats_core::plan::Plan;

use crate::config::Config;
use crate::store::{Store, unix_now};
use crate::walletd::WalletCtx;
use crate::{ui, walletd};

pub fn run(
    store: &Store,
    config: &Config,
    network: Network,
    address: &str,
    amount: u64,
    fee_rate: Option<u64>,
    json: bool,
) -> Result<()> {
    let mut ctx = walletd::open(store, config, network)?;
    if let Err(err) = ctx.sync() {
        eprintln!("✗ sync failed — planning on cached state ({err:#})");
    }
    let plan = build(&mut ctx, address, amount, fee_rate)?;
    ctx.persist()?;
    store.save_plan(ctx.net_name, &plan)?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": plan.id,
                "recipient": plan.recipient,
                "amount_sat": plan.amount_sat,
                "fee_sat": plan.fee_sat,
                "total_sat": plan.total_sat(),
            })
        );
    } else {
        print_block(&plan);
        println!();
        ui::ok(&format!("plan saved  {}", plan.id));
        ui::dim("next: sats sign");
    }
    Ok(())
}

/// Parse, estimate, and build — shared by `plan` and `send`.
pub fn build(
    ctx: &mut WalletCtx,
    address: &str,
    amount: u64,
    fee_rate: Option<u64>,
) -> Result<Plan> {
    let addr = Address::from_str(address)
        .map_err(|e| anyhow!("invalid address: {e}"))?
        .require_network(ctx.network)
        .map_err(|_| anyhow!("address is not valid for {}", ctx.net_name))?;
    let rate = match fee_rate {
        Some(sat_vb) => {
            let sat_vb = u32::try_from(sat_vb).unwrap_or(u32::MAX).max(1);
            FeeRate::from_sat_per_vb_u32(sat_vb)
        }
        None => ctx
            .estimate_fee_rate(2)
            .context("cannot estimate fee — pass --fee-rate")?,
    };
    Ok(engine::build_plan(
        &mut ctx.wallet,
        &addr,
        Amount::from_sat(amount),
        rate,
        &[],
        ctx.net_name,
        unix_now(),
    )?)
}

pub fn print_block(plan: &Plan) {
    ui::sat_rows(&[
        ("Send", plan.amount_sat),
        ("Fee", plan.fee_sat),
        ("Total", plan.total_sat()),
    ]);
}
