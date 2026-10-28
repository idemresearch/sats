//! Human-authorized Alkanes transfers.
//!
//! A surface stages, renders the immutable review, and obtains human approval
//! before consuming the staged operation with a signer factory. Staging has no
//! signer or key. The factory is invoked only in commit, once. Agent requests
//! must use request::execute; this module provides no grant authority. There
//! is no generic contract call: sats signs only operations it composes and
//! verifies itself.

use super::transfer::{TransferRequest, TransferReview};
use anyhow::{Context, Result, bail};
use sats_core::bitcoin::{FeeRate, Network, Psbt};
use sats_core::plan::{PreparedSpend, TransactionRecord, TxOrigin};
use sats_core::signer::Signer;

use crate::provider::Services;
use crate::spend;
use crate::store::Store;
use crate::walletd::WalletCtx;

/// An unsigned, checked operation. Private fields and a consuming commit bind
/// approval to the reviewed PSBT, wallet, network and provider. Not Clone.
#[must_use = "render the review and obtain human authorization before commit"]
pub struct StagedOperation<'a> {
    store: &'a Store,
    services: &'a Services,
    ctx: WalletCtx,
    prepared: PreparedSpend,
    review: TransferReview,
}

pub struct ExecutionOutcome {
    pub transaction: TransactionRecord,
    /// A signature is persisted even when broadcast fails. Only rebroadcast
    /// the saved transaction; never rebuild and sign again automatically.
    pub broadcast_error: Option<anyhow::Error>,
}

impl StagedOperation<'_> {
    pub fn review(&self) -> &TransferReview {
        &self.review
    }

    pub fn network(&self) -> Network {
        self.ctx.network
    }

    pub fn wallet_path(&self) -> std::path::PathBuf {
        self.store.wallet_db_path(self.ctx.net_name)
    }

    /// Call only after a surface has rendered review() and obtained explicit
    /// human approval. The factory obtains a signer using that surface's own
    /// authorization mechanism. The API accepts no mnemonic or password;
    /// signing material stays encapsulated behind the Signer boundary.
    pub fn commit<S: Signer>(
        mut self,
        surface: &str,
        signer_factory: impl FnOnce() -> Result<S>,
    ) -> Result<ExecutionOutcome> {
        super::transfer::recheck(&self.ctx, self.services, self.prepared.psbt(), &self.review)?;
        let mut record = {
            let mut signer = signer_factory()?;
            spend::sign_with(self.prepared, &mut signer)
                .context("signer invoked: a signature may exist — do not automatically retry")?
        }
        .with_origin(TxOrigin {
            surface: surface.into(),
            agent: None,
            request_id: None,
            intent_digest: None,
        });
        self.store.save_transaction(self.ctx.net_name, &record)
            .context("signed transaction could not be persisted — do not automatically retry or sign again")?;
        let broadcast_error =
            spend::broadcast_record(self.store, &mut self.ctx, self.services, &mut record).err();
        Ok(ExecutionOutcome {
            transaction: record,
            broadcast_error,
        })
    }
}

pub(crate) fn fee_rate(services: &Services, requested: Option<u64>) -> Result<FeeRate> {
    match requested {
        Some(rate) => Ok(FeeRate::from_sat_per_vb_u32(
            u32::try_from(rate).unwrap_or(u32::MAX).max(1),
        )),
        None => services
            .estimate_fee_rate()
            .context("cannot estimate fee — pass --fee-rate"),
    }
}

/// PSBT input facts must agree with this wallet's own UTXOs. Both protocol
/// verifiers receive those independently obtained values.
pub(crate) fn input_values(ctx: &WalletCtx, psbt: &Psbt) -> Result<Vec<u64>> {
    let mut values = Vec::new();
    for (i, input) in psbt.unsigned_tx.input.iter().enumerate() {
        let utxo = ctx
            .wallet
            .list_unspent()
            .find(|u| u.outpoint == input.previous_output)
            .with_context(|| format!("input {} is not a wallet UTXO", input.previous_output))?;
        if psbt
            .inputs
            .get(i)
            .and_then(|input| input.witness_utxo.as_ref())
            != Some(&utxo.txout)
        {
            bail!("PSBT input {i} disagrees with the wallet UTXO");
        }
        values.push(utxo.txout.value.to_sat());
    }
    Ok(values)
}

pub fn stage_transfer<'a>(
    store: &'a Store,
    network: Network,
    services: &'a Services,
    args: &TransferRequest,
) -> Result<StagedOperation<'a>> {
    let transfer = super::transfer::prepare(store, network, services, args)?;
    Ok(StagedOperation {
        store,
        services,
        ctx: transfer.ctx,
        prepared: transfer.prepared,
        review: transfer.review,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChainChoice, Config, EsploraConfig, RedactedUrl};
    use crate::walletd;
    use anyhow::anyhow;
    use sats_core::bdk_wallet::chain::{BlockId, ConfirmationBlockTime};
    use sats_core::bdk_wallet::test_utils::{insert_checkpoint, receive_output};
    use sats_core::bitcoin::Amount;
    use sats_core::bitcoin::hashes::Hash;
    use sats_core::bitcoin::{BlockHash, Psbt};
    use sats_core::error::SignerError;
    use sats_core::{seed, signer::LocalSigner};
    use std::cell::Cell;

    const MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    struct Fixture {
        _dir: tempfile::TempDir,
        store: Store,
        services: Services,
        mockdata: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path());
            let mockdata = dir.path().join("mock");
            std::fs::create_dir(&mockdata).unwrap();
            std::fs::write(mockdata.join("alkanes-simulate.json"), r#"{"status":0}"#).unwrap();
            let mut config = Config::default();
            config.signet.chain = Some(ChainChoice::Esplora);
            config.signet.esplora = Some(EsploraConfig {
                url: RedactedUrl(format!("file://{}", mockdata.display())),
                bearer: None,
            });
            let services = crate::provider::resolve(&config, None, Network::Signet).unwrap();
            let mnemonic = seed::parse_mnemonic(MNEMONIC).unwrap();
            let (ext, int) = seed::public_descriptors(&mnemonic, Network::Signet).unwrap();
            walletd::create(&store, Network::Signet, ext, int).unwrap();
            let mut ctx = walletd::open(&store, Network::Signet).unwrap();
            let block = BlockId {
                height: 10,
                hash: BlockHash::all_zeros(),
            };
            insert_checkpoint(&mut ctx.wallet, block);
            receive_output(
                &mut ctx.wallet,
                Amount::from_sat(100_000),
                ConfirmationBlockTime {
                    block_id: block,
                    confirmation_time: 100,
                },
            );
            ctx.persist().unwrap();
            // The wallet's only output carries the asset every test sends.
            let outpoint = ctx.wallet.list_unspent().next().unwrap().outpoint;
            std::fs::write(
                mockdata.join("alkanes-outpoints.json"),
                serde_json::json!({outpoint.to_string(): {"32:0":"5000","2:0":"50"}}).to_string(),
            )
            .unwrap();
            Self {
                _dir: dir,
                store,
                services,
                mockdata,
            }
        }

        fn stage(&self) -> StagedOperation<'_> {
            stage_transfer(&self.store, Network::Signet, &self.services, &request()).unwrap()
        }

        fn assert_no_spend(&self) {
            assert!(self.store.list_transactions("signet").unwrap().is_empty());
            assert!(!self.mockdata.join("broadcasts.log").exists());
        }
    }

    const RECIPIENT: &str = "tb1pvlnw9n2zuefmxzwmuz0763uajw8nmaattkhd8002g3ekejjspxtshu2q9n";

    fn request() -> TransferRequest {
        TransferRequest {
            asset: "32:0".parse().unwrap(),
            amount: 1000,
            address: RECIPIENT.into(),
            fee_rate: Some(2),
            postage_sat: 546,
        }
    }

    struct Probe<'a> {
        fixture: &'a Fixture,
        calls: &'a Cell<usize>,
        fail: bool,
        unfinalized: bool,
        change_transaction: bool,
    }

    impl Signer for Probe<'_> {
        fn name(&self) -> &'static str {
            "human-probe"
        }
        fn sign(&mut self, psbt: &mut Psbt) -> std::result::Result<bool, SignerError> {
            self.fixture.assert_no_spend();
            self.calls.set(self.calls.get() + 1);
            let mut signer =
                LocalSigner::new(seed::parse_mnemonic(MNEMONIC).unwrap(), Network::Signet).unwrap();
            signer.sign(psbt)?;
            if self.fail {
                return Err(SignerError::Unfinalized);
            }
            if self.change_transaction {
                psbt.unsigned_tx.output[1].value = Amount::from_sat(999);
            }
            Ok(!self.unfinalized)
        }
    }

    impl Drop for Probe<'_> {
        fn drop(&mut self) {
            // Key holders must be gone before persistence and broadcasting.
            self.fixture.assert_no_spend();
        }
    }

    #[test]
    fn stage_is_unsigned_and_commit_consumes_the_exact_reviewed_operation() {
        let fx = Fixture::new();
        let stage = fx.stage();
        fx.assert_no_spend();
        let review = stage.review();
        let fee = review.summary.fee_sat;
        assert_eq!(review.amount, 1000);
        let mut detached = review.clone();
        detached.amount = 1;
        detached.summary.fee_sat = 0;
        let calls = Cell::new(0);
        let outcome = stage
            .commit("test-human", || {
                Ok(Probe {
                    fixture: &fx,
                    calls: &calls,
                    fail: false,
                    unfinalized: false,
                    change_transaction: false,
                })
            })
            .unwrap();
        assert_eq!(calls.get(), 1);
        assert!(outcome.broadcast_error.is_none());
        assert_eq!(outcome.transaction.fee_sat, fee);
        assert_eq!(outcome.transaction.recipient, RECIPIENT);
        assert_eq!(
            outcome.transaction.origin.as_ref().unwrap().surface,
            "test-human"
        );
        let stored = fx
            .store
            .load_transaction("signet", &outcome.transaction.txid)
            .unwrap();
        assert_eq!(stored.tx_hex, outcome.transaction.tx_hex);
        assert!(fx.mockdata.join("broadcasts.log").exists());
    }

    #[test]
    fn transfer_index_changes_stop_before_the_human_signer_factory() {
        let fx = Fixture::new();
        let staged = fx.stage();
        std::fs::write(fx.mockdata.join("alkanes-outpoints.json"), "{}").unwrap();
        let result = staged.commit::<LocalSigner>("test", || panic!("must not construct signer"));
        assert!(result.is_err());
        fx.assert_no_spend();
    }

    #[test]
    fn signer_factory_failure_never_signs_persists_or_broadcasts() {
        let fx = Fixture::new();
        let result = fx
            .stage()
            .commit::<LocalSigner>("test", || Err(anyhow!("authorization cancelled")));
        assert!(format!("{:#}", result.err().unwrap()).contains("authorization cancelled"));
        fx.assert_no_spend();
    }

    #[test]
    fn signer_failure_unfinalized_and_changed_transaction_never_broadcast_or_retry() {
        for (fail, unfinalized, change_transaction) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let fx = Fixture::new();
            let calls = Cell::new(0);
            let result = fx.stage().commit("test", || {
                Ok(Probe {
                    fixture: &fx,
                    calls: &calls,
                    fail,
                    unfinalized,
                    change_transaction,
                })
            });
            assert!(format!("{:#}", result.err().unwrap()).contains("do not automatically retry"));
            assert_eq!(calls.get(), 1);
            fx.assert_no_spend();
        }
    }

    #[test]
    fn signature_is_persisted_on_broadcast_failure() {
        let fx = Fixture::new();
        std::fs::write(fx.mockdata.join("broadcast-fail"), "offline").unwrap();
        let calls = Cell::new(0);
        let result = fx
            .stage()
            .commit("test", || {
                Ok(Probe {
                    fixture: &fx,
                    calls: &calls,
                    fail: false,
                    unfinalized: false,
                    change_transaction: false,
                })
            })
            .unwrap();
        assert!(result.broadcast_error.is_some());
        assert_eq!(calls.get(), 1);
        let stored = fx
            .store
            .load_transaction("signet", &result.transaction.txid)
            .unwrap();
        assert_eq!(stored.status, sats_core::plan::TransactionStatus::Pending);
        assert_eq!(stored.tx_hex, result.transaction.tx_hex);
        assert!(!stored.tx_hex.is_empty());
        assert!(!fx.mockdata.join("broadcasts.log").exists());
    }

    #[test]
    fn signature_is_never_broadcast_when_persistence_fails() {
        let fx = Fixture::new();
        let stage = fx.stage();
        std::fs::write(fx.store.transactions_dir("signet"), "not a directory").unwrap();
        let calls = Cell::new(0);
        // This probe checks broadcast ordering without trying to enumerate the
        // deliberately damaged transaction directory in its Drop implementation.
        struct LocalProbe<'a>(&'a Cell<usize>);
        impl Signer for LocalProbe<'_> {
            fn name(&self) -> &'static str {
                "persist-probe"
            }
            fn sign(&mut self, psbt: &mut Psbt) -> std::result::Result<bool, SignerError> {
                self.0.set(self.0.get() + 1);
                LocalSigner::new(seed::parse_mnemonic(MNEMONIC).unwrap(), Network::Signet)
                    .unwrap()
                    .sign(psbt)
            }
        }
        let error = stage
            .commit("test", || Ok(LocalProbe(&calls)))
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("could not be persisted"));
        assert_eq!(calls.get(), 1);
        assert!(!fx.mockdata.join("broadcasts.log").exists());
    }
}
