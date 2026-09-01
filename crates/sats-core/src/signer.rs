//! The signer boundary. Everything above this trait is portable; each
//! implementation is environment-specific. V1 ships `LocalSigner`;
//! hardware and passkey backends are future implementations of the same
//! trait.

use bdk_wallet::SignOptions;
use bdk_wallet::bitcoin::{Network, Psbt};
use bip39::Mnemonic;

use crate::error::SignerError;
use crate::seed;

pub trait Signer {
    fn name(&self) -> &'static str;

    /// Sign every input this signer can. Returns `true` when the PSBT is
    /// fully signed and finalized.
    fn sign(&mut self, psbt: &mut Psbt) -> Result<bool, SignerError>;
}

/// Signs with an in-memory BIP-39 mnemonic via an ephemeral BDK wallet.
/// The private keys never touch persistence.
pub struct LocalSigner {
    mnemonic: Mnemonic,
    network: Network,
}

impl LocalSigner {
    pub fn new(mnemonic: Mnemonic, network: Network) -> Self {
        LocalSigner { mnemonic, network }
    }
}

impl Signer for LocalSigner {
    fn name(&self) -> &'static str {
        "local"
    }

    fn sign(&mut self, psbt: &mut Psbt) -> Result<bool, SignerError> {
        let wallet = seed::signing_wallet(&self.mnemonic, self.network)?;
        Ok(wallet.sign(psbt, SignOptions::default())?)
    }
}

#[cfg(test)]
mod tests {
    use bdk_wallet::bitcoin::hashes::Hash;
    use bdk_wallet::bitcoin::{Amount, BlockHash, FeeRate};
    use bdk_wallet::chain::{BlockId, ConfirmationBlockTime};
    use bdk_wallet::test_utils::{insert_checkpoint, receive_output};
    use bdk_wallet::{KeychainKind, Wallet};

    use super::*;
    use crate::engine::build_plan;
    use crate::plan::TransactionStatus;

    const MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    /// The full offline product loop: a watch-only wallet plans, the
    /// LocalSigner signs, and the result finalizes into a broadcastable tx.
    #[test]
    fn watch_only_plan_signs_and_finalizes() {
        let mnemonic = seed::parse_mnemonic(MNEMONIC).unwrap();
        let (ext, int) = seed::public_descriptors(&mnemonic, Network::Signet).unwrap();
        let mut wallet = Wallet::create(ext, int)
            .network(Network::Signet)
            .create_wallet_no_persist()
            .unwrap();

        // Fund the watch-only wallet with a confirmed fake output.
        let block_1000 = BlockId {
            height: 1000,
            hash: BlockHash::all_zeros(),
        };
        insert_checkpoint(&mut wallet, block_1000);
        insert_checkpoint(
            &mut wallet,
            BlockId {
                height: 2000,
                hash: BlockHash::all_zeros(),
            },
        );
        receive_output(
            &mut wallet,
            Amount::from_sat(100_000),
            ConfirmationBlockTime {
                block_id: block_1000,
                confirmation_time: 100,
            },
        );

        let recipient = wallet.reveal_next_address(KeychainKind::External).address;
        let plan = build_plan(
            &mut wallet,
            &recipient,
            Amount::from_sat(25_000),
            FeeRate::from_sat_per_vb_u32(2),
            &[],
            "signet",
            1_700_000_000,
        )
        .unwrap();

        assert_eq!(plan.amount_sat, 25_000);
        assert!(plan.fee_sat > 0, "fee must be computed");
        assert_eq!(plan.total_sat(), plan.amount_sat + plan.fee_sat);
        assert_eq!(plan.id.len(), 8);

        // The watch-only wallet itself must NOT be able to sign.
        let mut psbt = plan.psbt().clone();
        let watch_only_result = wallet.sign(&mut psbt, SignOptions::default()).unwrap();
        assert!(!watch_only_result, "watch-only wallet must not finalize");

        // The LocalSigner must.
        let mut psbt = plan.psbt().clone();
        let mut signer = LocalSigner::new(mnemonic, Network::Signet);
        assert_eq!(signer.name(), "local");
        let finalized = signer.sign(&mut psbt).unwrap();
        assert!(finalized, "signed PSBT must finalize");

        let tx = psbt.clone().extract_tx().unwrap();
        assert!(tx.output.iter().any(|o| o.value.to_sat() == 25_000));

        let record = plan.into_transaction(psbt).unwrap();
        assert_eq!(record.status, TransactionStatus::Pending);
        assert_eq!(record.txid, tx.compute_txid().to_string());
        assert_eq!(record.tx().unwrap(), tx);
        assert!(!record.tx_hex.is_empty());
        let record_json = serde_json::to_string(&record).unwrap();
        assert!(record_json.contains("tx_hex"));
        assert!(!record_json.contains("psbt"));
    }

    /// Planning more than the balance is a typed failure, not a panic.
    #[test]
    fn overspend_fails_cleanly() {
        let mnemonic = seed::parse_mnemonic(MNEMONIC).unwrap();
        let (ext, int) = seed::public_descriptors(&mnemonic, Network::Signet).unwrap();
        let mut wallet = Wallet::create(ext, int)
            .network(Network::Signet)
            .create_wallet_no_persist()
            .unwrap();
        let recipient = wallet.reveal_next_address(KeychainKind::External).address;
        let result = build_plan(
            &mut wallet,
            &recipient,
            Amount::from_sat(25_000),
            FeeRate::from_sat_per_vb_u32(2),
            &[],
            "signet",
            0,
        );
        assert!(result.is_err());
    }
}
