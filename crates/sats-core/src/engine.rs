//! Transaction planning on a caller-owned wallet. Pure of IO: the wallet's
//! UTXO view is whatever the caller last synced, and protection facts
//! (outpoints to avoid) are plain inputs the shell supplies.

use std::collections::HashSet;

use bdk_wallet::Wallet;
use bdk_wallet::bitcoin::{Address, Amount, FeeRate, OutPoint};

use crate::error::PlanError;
use crate::plan::{Plan, PlanStatus};

/// Classic inscription postage values. UTXOs at exactly these amounts are
/// likely to be carrying an ordinal inscription and are excluded from
/// automatic selection by default (see [`dust_suspects`]).
pub const POSTAGE_VALUES: [u64; 2] = [546, 330];

/// Outpoints whose value matches a classic inscription postage amount
/// (546 or 330 sats). A heuristic safety floor, not asset detection:
/// false positives are recoverable by the caller opting out, false
/// negatives are what indexer-backed guards are for.
pub fn dust_suspects(utxos: impl IntoIterator<Item = (OutPoint, Amount)>) -> Vec<OutPoint> {
    utxos
        .into_iter()
        .filter(|(_, value)| POSTAGE_VALUES.contains(&value.to_sat()))
        .map(|(outpoint, _)| outpoint)
        .collect()
}

/// Build an unsigned spend plan. Reveals a change address on the wallet, so
/// callers with persistence should persist afterwards.
///
/// `unspendable` outpoints are excluded from coin selection. This is only a
/// guarantee while nothing calls `TxBuilder::add_utxo`, which takes priority
/// over the unspendable set — this builder never does.
pub fn build_plan(
    wallet: &mut Wallet,
    recipient: &Address,
    amount: Amount,
    fee_rate: FeeRate,
    unspendable: &[OutPoint],
    network: &str,
    now_unix: u64,
) -> Result<Plan, PlanError> {
    let excluded_utxos = {
        let excluded: HashSet<OutPoint> = unspendable.iter().copied().collect();
        wallet
            .list_unspent()
            .filter(|u| excluded.contains(&u.outpoint))
            .count() as u64
    };
    let mut builder = wallet.build_tx();
    builder.add_recipient(recipient.script_pubkey(), amount);
    builder.fee_rate(fee_rate);
    builder.unspendable(unspendable.to_vec());
    let psbt = builder.finish().map_err(Box::new)?;

    let fee_sat = psbt.fee()?.to_sat();
    let id = psbt.unsigned_tx.compute_txid().to_string()[..8].to_string();
    Ok(Plan {
        id,
        network: network.to_string(),
        recipient: recipient.to_string(),
        amount_sat: amount.to_sat(),
        fee_sat,
        created_at: now_unix,
        status: PlanStatus::Unsigned,
        psbt: psbt.to_string(),
        excluded_utxos,
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use bdk_wallet::bitcoin::{BlockHash, Network, hashes::Hash};
    use bdk_wallet::chain::{BlockId, ConfirmationBlockTime};
    use bdk_wallet::test_utils::{insert_checkpoint, receive_output};
    use bdk_wallet::{KeychainKind, Wallet};

    use super::*;
    use crate::seed;

    const MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn funded_wallet(values: &[u64]) -> (Wallet, Vec<OutPoint>) {
        let mnemonic = seed::parse_mnemonic(MNEMONIC).unwrap();
        let (ext, int) = seed::public_descriptors(&mnemonic, Network::Signet).unwrap();
        let mut wallet = Wallet::create(ext, int)
            .network(Network::Signet)
            .create_wallet_no_persist()
            .unwrap();
        let block_900 = BlockId {
            height: 900,
            hash: BlockHash::all_zeros(),
        };
        insert_checkpoint(&mut wallet, block_900);
        insert_checkpoint(
            &mut wallet,
            BlockId {
                height: 1_000,
                hash: BlockHash::all_zeros(),
            },
        );
        let outpoints = values
            .iter()
            .map(|v| {
                receive_output(
                    &mut wallet,
                    Amount::from_sat(*v),
                    ConfirmationBlockTime {
                        block_id: block_900,
                        confirmation_time: 100,
                    },
                )
            })
            .collect();
        (wallet, outpoints)
    }

    fn recipient(wallet: &mut Wallet) -> Address {
        wallet.reveal_next_address(KeychainKind::External).address
    }

    #[test]
    fn unspendable_outpoints_are_not_selected() {
        let (mut wallet, outs) = funded_wallet(&[50_000, 40_000]);
        let addr = recipient(&mut wallet);
        let plan = build_plan(
            &mut wallet,
            &addr,
            Amount::from_sat(10_000),
            FeeRate::from_sat_per_vb_u32(2),
            &outs[..1],
            "signet",
            0,
        )
        .unwrap();
        let tx = plan.tx().unwrap();
        assert!(tx.input.iter().all(|i| i.previous_output != outs[0]));
        assert_eq!(plan.excluded_utxos, 1);
    }

    #[test]
    fn all_excluded_means_insufficient_funds() {
        let (mut wallet, outs) = funded_wallet(&[50_000]);
        let addr = recipient(&mut wallet);
        let err = build_plan(
            &mut wallet,
            &addr,
            Amount::from_sat(10_000),
            FeeRate::from_sat_per_vb_u32(2),
            &outs,
            "signet",
            0,
        )
        .unwrap_err();
        assert!(matches!(err, PlanError::CreateTx(_)));
    }

    #[test]
    fn excluded_count_ignores_foreign_outpoints() {
        let (mut wallet, _) = funded_wallet(&[50_000]);
        let addr = recipient(&mut wallet);
        let foreign = OutPoint::from_str(
            "aa00000000000000000000000000000000000000000000000000000000000000:0",
        )
        .unwrap();
        let plan = build_plan(
            &mut wallet,
            &addr,
            Amount::from_sat(10_000),
            FeeRate::from_sat_per_vb_u32(2),
            &[foreign],
            "signet",
            0,
        )
        .unwrap();
        assert_eq!(plan.excluded_utxos, 0);
    }

    #[test]
    fn dust_suspects_matches_postage_exactly() {
        let op = |n: u32| OutPoint {
            txid: bdk_wallet::bitcoin::Txid::all_zeros(),
            vout: n,
        };
        let utxos = vec![
            (op(0), Amount::from_sat(546)),
            (op(1), Amount::from_sat(330)),
            (op(2), Amount::from_sat(547)),
            (op(3), Amount::from_sat(1_000)),
            (op(4), Amount::from_sat(0)),
        ];
        assert_eq!(dust_suspects(utxos), vec![op(0), op(1)]);
    }
}
