//! Human-authorized Alkanes execution.
//!
//! A surface stages, renders the immutable review, and obtains human approval
//! before consuming the staged operation with a signer factory. Staging has no
//! signer or key. The factory is invoked only in commit, once. Agent requests
//! must use request::execute; this module provides no grant authority.

use std::collections::BTreeSet;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use sats_alkanes::build::build_execute_plan;
use sats_alkanes::call::AlkaneCall;
use sats_alkanes::delta::SimulationView;
use sats_alkanes::id::AlkaneId;
use sats_alkanes::transfer::{Sheet, TransferSummary, build_transfer_plan, summarize};
use sats_core::bdk_wallet::KeychainKind;
use sats_core::bitcoin::{Address, Amount, FeeRate, Network, OutPoint, Psbt};
use sats_core::engine;
use sats_core::plan::{PreparedSpend, TransactionRecord, TxOrigin};
use sats_core::signer::Signer;

use crate::assets;
use crate::provider::Services;
use crate::spend;
use crate::store::{Store, unix_now};
use crate::walletd::{self, WalletCtx};

use super::{check_services, scan_wallet, simulate};

pub struct ExecuteRequest {
    pub call: AlkaneCall,
    pub fee_rate: Option<u64>,
    pub postage_sat: u64,
}

pub struct TransferRequest {
    pub asset: AlkaneId,
    pub amount: u128,
    pub address: String,
    pub fee_rate: Option<u64>,
    pub postage_sat: u64,
}

#[derive(Debug, Clone)]
pub struct ExecuteReview {
    pub call: AlkaneCall,
    pub simulation: SimulationView,
    pub pointer_address: String,
    pub postage_sat: u64,
    pub fee_sat: u64,
    /// Postage stays in the wallet; only the fee leaves it.
    pub external_sat: u64,
    pub excluded_utxos: u64,
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

#[derive(Debug, Clone)]
pub enum OperationReview {
    Execute(ExecuteReview),
    Transfer(TransferReview),
}

/// An unsigned, checked operation. Private fields and a consuming commit bind
/// approval to the reviewed PSBT, wallet, network and provider. Not Clone.
#[must_use = "render the review and obtain human authorization before commit"]
pub struct StagedOperation<'a> {
    store: &'a Store,
    services: &'a Services,
    ctx: WalletCtx,
    prepared: PreparedSpend,
    review: OperationReview,
}

pub struct ExecutionOutcome {
    pub transaction: TransactionRecord,
    /// A signature is persisted even when broadcast fails. Only rebroadcast
    /// the saved transaction; never rebuild and sign again automatically.
    pub broadcast_error: Option<anyhow::Error>,
}

impl StagedOperation<'_> {
    pub fn review(&self) -> &OperationReview {
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

fn fee_rate(services: &Services, requested: Option<u64>) -> Result<FeeRate> {
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

pub fn stage_execute<'a>(
    store: &'a Store,
    network: Network,
    services: &'a Services,
    args: &ExecuteRequest,
) -> Result<StagedOperation<'a>> {
    check_services(network, services)?;
    let simulation = simulate(services, &args.call)?;
    simulation.require_success()?;
    let mut ctx = walletd::open(store, network)?;
    services
        .sync_wallet(&mut ctx)
        .map_err(|e| anyhow!("{e} — refusing to plan on stale state"))?;
    let utxos: Vec<(OutPoint, Amount)> = ctx
        .wallet
        .list_unspent()
        .map(|u| (u.outpoint, u.txout.value))
        .collect();
    let mut excluded: BTreeSet<OutPoint> = engine::dust_suspects(utxos).into_iter().collect();
    excluded.extend(assets::excluded_for_send(&ctx, services)?.unwrap_or_default());
    let pointer = ctx.wallet.reveal_next_address(KeychainKind::External);
    let prepared = build_execute_plan(
        &mut ctx.wallet,
        &args.call,
        &pointer.address,
        args.postage_sat,
        fee_rate(services, args.fee_rate)?,
        &excluded.into_iter().collect::<Vec<_>>(),
        ctx.net_name,
        unix_now(),
    )?;
    ctx.persist()?;
    let tx = &prepared.psbt().unsigned_tx;
    let summary = sats_alkanes::execute::summarize(
        tx,
        &input_values(&ctx, prepared.psbt())?,
        &args.call,
        args.postage_sat,
        |i| {
            ctx.wallet
                .is_mine(tx.output[i as usize].script_pubkey.clone())
        },
    )?;
    if summary.fee_sat != prepared.fee_sat || summary.postage_sat != prepared.amount_sat {
        bail!("prepared execution disagrees with the verified transaction");
    }
    let review = ExecuteReview {
        call: args.call.clone(),
        simulation,
        pointer_address: Address::from_script(&tx.output[1].script_pubkey, network)?.to_string(),
        postage_sat: summary.postage_sat,
        fee_sat: summary.fee_sat,
        external_sat: 0,
        excluded_utxos: prepared.excluded_utxos,
    };
    Ok(StagedOperation {
        store,
        services,
        ctx,
        prepared,
        review: OperationReview::Execute(review),
    })
}

pub fn stage_transfer<'a>(
    store: &'a Store,
    network: Network,
    services: &'a Services,
    args: &TransferRequest,
) -> Result<StagedOperation<'a>> {
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
        .filter_map(|o| {
            let held: u128 = o
                .balances
                .iter()
                .filter(|b| b.id == asset)
                .map(|b| b.amount)
                .sum();
            (held > 0).then_some((o, held))
        })
        .collect();
    holders.sort_by_key(|h| std::cmp::Reverse(h.1));
    let held_total: u128 = holders.iter().map(|(_, held)| held).sum();
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
        covered += held;
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

    // Check the transaction itself.
    let psbt = prepared.psbt();
    let tx = &psbt.unsigned_tx;
    let input_sat = input_values(&ctx, psbt)?;
    let mut input_sheets = Vec::new();
    for txin in &tx.input {
        let scanned_output = scanned
            .get(&txin.previous_output)
            .filter(|o| o.known)
            .ok_or_else(|| anyhow!("input {} was not indexed", txin.previous_output))?;
        let mut sheet = Sheet::new();
        for b in &scanned_output.balances {
            *sheet.entry(b.id).or_default() += b.amount;
        }
        input_sheets.push(sheet);
    }
    let summary = summarize(
        tx,
        &input_sat,
        &input_sheets,
        asset,
        args.amount,
        &recipient.script_pubkey(),
        |vout| {
            ctx.wallet
                .is_mine(tx.output[vout as usize].script_pubkey.clone())
        },
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
    Ok(StagedOperation {
        store,
        services,
        ctx,
        prepared,
        review: OperationReview::Transfer(review),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChainChoice, Config, EsploraConfig, RedactedUrl};
    use sats_core::bdk_wallet::chain::{BlockId, ConfirmationBlockTime};
    use sats_core::bdk_wallet::test_utils::{insert_checkpoint, receive_output};
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
            std::fs::write(mockdata.join("alkanes-outpoints.json"), "{}").unwrap();
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
            Self {
                _dir: dir,
                store,
                services,
                mockdata,
            }
        }

        fn stage(&self) -> StagedOperation<'_> {
            stage_execute(&self.store, Network::Signet, &self.services, &request()).unwrap()
        }

        fn assert_no_spend(&self) {
            assert!(self.store.list_transactions("signet").unwrap().is_empty());
            assert!(!self.mockdata.join("broadcasts.log").exists());
        }
    }

    fn request() -> ExecuteRequest {
        ExecuteRequest {
            call: AlkaneCall {
                target: AlkaneId { block: 2, tx: 1 },
                inputs: vec![77],
            },
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
        let OperationReview::Execute(review) = stage.review() else {
            panic!("execute review")
        };
        let fee = review.fee_sat;
        assert_eq!(review.external_sat, 0);
        let mut detached = review.clone();
        detached.call.target.tx = 99;
        detached.fee_sat = 0;
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
        assert_eq!(outcome.transaction.recipient, "alkanes:2:1");
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

    #[test]
    fn invalid_simulation_sync_and_network_fail_before_staging() {
        let fx = Fixture::new();
        for raw in [
            r#"{"status":7}"#,
            "null",
            "{}",
            r#"{"status":0,"error":"revert"}"#,
        ] {
            std::fs::write(fx.mockdata.join("alkanes-simulate.json"), raw).unwrap();
            let error = stage_execute(&fx.store, Network::Signet, &fx.services, &request())
                .err()
                .unwrap();
            assert!(format!("{error:#}").contains("refusing execution"));
            fx.assert_no_spend();
        }
        for network in [Network::Bitcoin, Network::Testnet4] {
            let error = stage_execute(&fx.store, network, &fx.services, &request())
                .err()
                .unwrap();
            assert!(
                !format!("{error:#}").contains("refusing execution"),
                "network checked before simulation"
            );
        }
        std::fs::write(fx.mockdata.join("alkanes-simulate.json"), r#"{"status":0}"#).unwrap();
        std::fs::write(fx.mockdata.join("sync-error"), "offline").unwrap();
        let error = stage_execute(&fx.store, Network::Signet, &fx.services, &request())
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("stale state"));
        fx.assert_no_spend();
    }
}
