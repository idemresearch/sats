//! Human-authorized execution of an agent request.
//!
//! Two phases, so a surface can put the real transaction in front of
//! the human before asking for their authorization:
//!
//! - [`stage`]: claim the request, prepare the transaction on the
//!   current chain state, derive what it actually pays from the wallet's
//!   own descriptors, and run the full ladder with the real fee. Nothing
//!   is reserved or signed; a policy refusal is recorded as `denied`.
//! - [`commit`]: reserve the budget and persist `signing` *before* the
//!   signer is invoked, sign through the signer factory (constructed
//!   only after the reservation is durable), persist the transaction
//!   before broadcasting, then settle to `sent` or `broadcast_pending`.
//!
//! The irreversible boundary is the signer invocation, not a successful
//! write afterwards. Before the signer is invoked, a failure refunds the
//! reservation and leaves the request re-approvable. Once the signer has
//! been invoked, only its own error report — "no signature was produced"
//! — permits a refund; any other failure, and any crash after `signing`
//! reached disk, leaves the request `unresolved`: never refunded and
//! never signed again by sats. A human resolves it.
//!
//! A request executes only under the grant instance that created it.
//! This module is the only path by which an agent-originated request
//! reaches the signer, and it is invoked only by a surface that has
//! obtained the human's authorization.

use anyhow::{Context, Result, bail};
use sats_core::authz::{Decision, DenyReason, Grant, SpendRequest, evaluate_send};
use sats_core::bitcoin::Network;
use sats_core::event::EventKind;
use sats_core::plan::{PreparedSpend, TxOrigin};
use sats_core::request::{AgentRequest, RequestState};
use sats_core::signer::Signer;
use sats_core::verify;

use crate::commands::prepare;
use crate::config::network_name;
use crate::provider::Services;
use crate::request::{bound_grant, describe_settled, journal, journal_soft, reconcile};
use crate::store::{RequestClaim, Store, now_checked, unix_now};
use crate::walletd::{self, WalletCtx};

/// A request prepared on current chain state and verified against its
/// recorded intent, awaiting the human's authorization to execute.
pub struct Staged {
    pub request: AgentRequest,
    /// The grant as it stood at staging, for display. The commit re-reads
    /// it under the lock.
    pub grant: Grant,
    /// What the prepared transaction pays and costs, derived from the
    /// PSBT, not from the caller.
    pub spend: SpendRequest,
    prepared: PreparedSpend,
    ctx: WalletCtx,
    _claim: RequestClaim,
}

/// What staging resolved to.
pub enum Stage {
    /// Prepared and inside the grant with the real fee: ready to commit.
    Ready(Box<Staged>),
    /// The grant refused it with the real fee, or the grant that created
    /// it is gone; recorded as `denied`.
    Denied(Box<AgentRequest>, DenyReason),
}

/// How an execution ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Sent {
        txid: String,
        amount_sat: u64,
        fee_sat: u64,
        remaining_sat: u64,
    },
    /// Signed and persisted; the broadcast failed. The reservation is
    /// final; `sats tx broadcast <txid>` retries.
    BroadcastPending {
        txid: String,
        amount_sat: u64,
        fee_sat: u64,
        message: String,
    },
    /// The signer was invoked and the outcome could not be made durable:
    /// a signature may exist. Never refunded, never signed again.
    Unresolved {
        txid: Option<String>,
        message: String,
    },
    /// Stopped before any signature could exist; refunded.
    Failed { message: String },
    /// The grant refused it at the final re-check; nothing drawn.
    Denied(DenyReason),
}

/// Claim, prepare, verify, and re-check a request with its real fee.
///
/// The request must be `pending_approval` or `failed`; an interrupted
/// `signing` record is reconciled first. The grant on file must be the
/// instance that created the request. Preparation runs on the human's
/// side of the boundary with no safety bypasses. Operational failures
/// (sync, guards, fee estimation) leave the request untouched so the
/// human can try again; a mismatch between the prepared transaction and
/// the recorded intent refuses without signing.
pub fn stage(
    store: &Store,
    network: Network,
    services: &Services,
    id_or_prefix: &str,
) -> Result<Stage> {
    let net_name = network_name(network);
    let request = store.find_agent_request(net_name, id_or_prefix)?;
    let request = reconcile(store, network, request)?;
    if !request.is_approvable() {
        bail!(
            "request {} is {} — nothing to approve",
            request.id,
            describe_settled(&request)
        );
    }
    let claim = store
        .claim_agent_request(net_name, &request.agent, &request.id)?
        .with_context(|| {
            format!(
                "request {} is signing right now — wait for it to settle",
                request.id
            )
        })?;
    let now = now_checked()?;
    // The grant that created the request, or nothing: a revoked or
    // re-issued grant makes every request filed under it non-executable.
    let grant = match bound_grant(store, net_name, &request)? {
        Ok(grant) => grant,
        Err(reason) => {
            let denied = record_denied(store, net_name, request, reason.clone(), now)?;
            return Ok(Stage::Denied(Box::new(denied), reason));
        }
    };
    // Precheck with the fee unknown, before spending a chain sync on a
    // request the grant now refuses.
    if let Decision::Deny(reason) = evaluate_send(
        &grant,
        &request.recipient,
        &SpendRequest {
            amount_sat: request.amount_sat,
            fee_sat: 0,
        },
        now,
    ) {
        let denied = record_denied(store, net_name, request, reason.clone(), now)?;
        return Ok(Stage::Denied(Box::new(denied), reason));
    }

    // Preparation: the shared pipeline, agent form — no dust or guard
    // bypass, and a failed sync is a hard stop.
    let mut ctx = walletd::open(store, network)?;
    let prepare_request =
        prepare::PrepareRequest::for_agent(&request.recipient, request.amount_sat);
    let prepared = prepare::build(&mut ctx, services, &prepare_request)?;
    ctx.persist()?;

    // What this transaction actually does, per our own descriptors. The
    // recorded intent committed to one exact payment; a PSBT that pays
    // anything else is not this request.
    let external = ctx
        .wallet
        .public_descriptor(sats_core::bdk_wallet::KeychainKind::External)
        .clone();
    let internal = ctx
        .wallet
        .public_descriptor(sats_core::bdk_wallet::KeychainKind::Internal)
        .clone();
    let derived = verify::derive_intent(prepared.psbt(), &external, &internal, network)
        .map_err(|e| anyhow::anyhow!("refusing to sign: {e}"))?;
    let (recipient, amount_sat) = derived
        .sole_recipient()
        .map_err(|e| anyhow::anyhow!("refusing to sign: {e}"))?;
    if recipient != request.recipient || amount_sat != request.amount_sat {
        bail!(
            "refusing to sign: the prepared transaction pays {} sat to {recipient}, \
             but this request is {} sat to {}",
            amount_sat,
            request.amount_sat,
            request.recipient
        );
    }
    let spend = SpendRequest {
        amount_sat: derived.sats_out,
        fee_sat: derived.fee_sat,
    };

    // The full ladder with the real fee. A refusal here is a grant
    // boundary the fee crossed: recorded, terminal.
    if let Decision::Deny(reason) = evaluate_send(&grant, &request.recipient, &spend, now) {
        let denied = record_denied(store, net_name, request, reason.clone(), now)?;
        return Ok(Stage::Denied(Box::new(denied), reason));
    }

    Ok(Stage::Ready(Box::new(Staged {
        request,
        grant,
        spend,
        prepared,
        ctx,
        _claim: claim,
    })))
}

/// Execute a staged request with the human's authorization.
///
/// `make_signer` is the signer boundary: it is invoked exactly once,
/// and only after the reservation and the `signing` record have been
/// persisted, so no denied or unauthorized request can ever construct a
/// signer, and a crash at any later point is recoverable as `unresolved`.
pub fn commit(
    store: &Store,
    network: Network,
    services: &Services,
    staged: Staged,
    make_signer: impl FnOnce() -> Result<Box<dyn Signer>>,
) -> Result<Outcome> {
    let net_name = network_name(network);
    let Staged {
        mut request,
        spend,
        prepared,
        mut ctx,
        _claim,
        ..
    } = staged;

    let (record, remaining_sat) = {
        // The whole decision is atomic with its persistence under the
        // grant lock: re-read the request and the grant, reserve, persist.
        let grant_lock = store.lock_grants(net_name)?;
        let now = now_checked()?;
        let fresh = store
            .load_agent_request(net_name, &request.agent, &request.id)?
            .with_context(|| format!("request {} disappeared", request.id))?;
        if !fresh.is_approvable() {
            bail!(
                "request {} is {} — nothing to approve",
                fresh.id,
                describe_settled(&fresh)
            );
        }
        request = fresh;
        let mut grant = match bound_grant(store, net_name, &request)? {
            Ok(grant) => grant,
            Err(reason) => {
                let denied = record_denied_locked(store, net_name, request, reason.clone(), now)?;
                drop(grant_lock);
                journal_denied(store, net_name, &denied, reason.clone());
                return Ok(Outcome::Denied(reason));
            }
        };
        if let Err(reason) = grant.reserve_send(&request.recipient, &spend, now) {
            let denied = record_denied_locked(store, net_name, request, reason.clone(), now)?;
            drop(grant_lock);
            journal_denied(store, net_name, &denied, reason.clone());
            return Ok(Outcome::Denied(reason));
        }

        // Reserve and persist BEFORE signing: once a signature exists the
        // money must be considered spent. Grant first, then the request:
        // a `signing` record on disk always means its draw is on the grant.
        store.save_grant(net_name, &grant)?;
        request.state = RequestState::Signing {
            approved_at: now,
            fee_sat: spend.fee_sat,
        };
        request.updated_at = now;
        store.save_agent_request(net_name, &request)?;
        journal_soft(store, net_name, &request, EventKind::Approved);

        // Everything from here to the signer invocation is still on the
        // near side of the boundary: this process knows the signer has
        // not run, so a failure refunds. On disk the request already says
        // `signing`, which is what a crash in this window must look like.
        let before_signer =
            |request: &mut AgentRequest, grant: &mut Grant, message: String| -> Result<Outcome> {
                grant.refund(&spend);
                record_failed_locked(store, net_name, request, &message, now)?;
                store.save_grant(net_name, grant)?;
                journal_soft(
                    store,
                    net_name,
                    request,
                    EventKind::Refunded {
                        total_sat: spend.total_sat(),
                    },
                );
                Ok(Outcome::Failed { message })
            };
        if let Err(err) = journal(
            store,
            net_name,
            &request,
            EventKind::Reserved {
                total_sat: spend.total_sat(),
                remaining_sat: grant.remaining_sat(),
            },
        ) {
            // Fail closed on a dead audit log, before the signer exists.
            let message = format!("cannot record reservation: {err:#}");
            return before_signer(&mut request, &mut grant, message);
        }
        let mut signer = match make_signer() {
            Ok(signer) => signer,
            Err(err) => {
                let message = format!("cannot construct the signer: {err:#}");
                return before_signer(&mut request, &mut grant, message);
            }
        };

        // The signer is invoked. From here, only its own report that no
        // signature was produced permits a refund.
        let mut psbt = prepared.psbt().clone();
        let unsigned_txid = psbt.unsigned_tx.compute_txid().to_string();
        let finalized = match signer.sign(&mut psbt) {
            Ok(finalized) => finalized,
            Err(err) => {
                let message = format!("signing failed: {err}");
                return before_signer(&mut request, &mut grant, message);
            }
        };
        drop(signer);
        let unresolved = |request: &mut AgentRequest, message: String| -> Result<Outcome> {
            // A signature may exist. The reservation stands and this
            // request is never signed again by sats.
            record_unresolved_locked(
                store,
                net_name,
                request,
                &message,
                Some(unsigned_txid.clone()),
                now,
            )?;
            Ok(Outcome::Unresolved {
                txid: Some(unsigned_txid.clone()),
                message,
            })
        };
        if !finalized {
            let message =
                "signer produced an unfinalized transaction — a partial signature may exist"
                    .to_string();
            return unresolved(&mut request, message);
        }
        let record = match prepared.into_transaction(psbt) {
            Ok(record) => record.with_origin(TxOrigin {
                surface: "agent".into(),
                agent: Some(request.agent.clone()),
                request_id: Some(request.id.clone()),
                intent_digest: Some(request.intent_digest.clone()),
            }),
            Err(err) => {
                let message = format!("cannot finalize the signed transaction: {err}");
                return unresolved(&mut request, message);
            }
        };
        // Persist the signature before it goes anywhere.
        if let Err(err) = store.save_transaction(net_name, &record) {
            let message = format!("cannot save the signed transaction: {err:#}");
            return unresolved(&mut request, message);
        }
        journal_soft(
            store,
            net_name,
            &request,
            EventKind::Signed {
                txid: record.txid.clone(),
            },
        );
        // A signature is durable: the reservation is final, no more
        // grant writes for this request.
        (record, grant.remaining_sat())
    };

    // Broadcast outside the lock: a slow provider must not stall the
    // wallet. A failure is never a refund.
    let mut record = record;
    match crate::spend::broadcast_record(store, &mut ctx, services, &mut record) {
        Ok(txid) => {
            let txid = txid.to_string();
            settle(
                store,
                net_name,
                &mut request,
                RequestState::Sent {
                    txid: txid.clone(),
                    fee_sat: spend.fee_sat,
                    at: unix_now(),
                },
                EventKind::Broadcast { txid: txid.clone() },
            );
            Ok(Outcome::Sent {
                txid,
                amount_sat: spend.amount_sat,
                fee_sat: spend.fee_sat,
                remaining_sat,
            })
        }
        Err(err) => {
            let txid = record.txid.clone();
            let message = format!(
                "broadcast failed after signing: {err:#} — budget reserved; \
                 retry with: sats tx broadcast {txid}"
            );
            settle(
                store,
                net_name,
                &mut request,
                RequestState::BroadcastPending {
                    txid: txid.clone(),
                    fee_sat: spend.fee_sat,
                    at: unix_now(),
                },
                EventKind::BroadcastFailed {
                    txid: txid.clone(),
                    message: message.clone(),
                },
            );
            Ok(Outcome::BroadcastPending {
                txid,
                amount_sat: spend.amount_sat,
                fee_sat: spend.fee_sat,
                message,
            })
        }
    }
}

/// Rewrite the request's state under a freshly taken grant lock and
/// journal the transition. Post-signature writes must not fail the
/// execution: warn.
fn settle(
    store: &Store,
    net_name: &str,
    request: &mut AgentRequest,
    state: RequestState,
    kind: EventKind,
) {
    request.state = state;
    request.updated_at = unix_now();
    match store.lock_grants(net_name) {
        Ok(_lock) => {
            if let Err(err) = store.save_agent_request(net_name, request) {
                eprintln!("⚠ request record not updated: {err:#}");
            }
        }
        Err(err) => eprintln!("⚠ request record not updated: {err:#}"),
    }
    journal_soft(store, net_name, request, kind);
}

fn journal_denied(store: &Store, net_name: &str, request: &AgentRequest, reason: DenyReason) {
    journal_soft(
        store,
        net_name,
        request,
        EventKind::Denied {
            deny: reason,
            stage: "execute".into(),
        },
    );
}

fn record_denied(
    store: &Store,
    net_name: &str,
    request: AgentRequest,
    reason: DenyReason,
    now: u64,
) -> Result<AgentRequest> {
    let denied = {
        let _lock = store.lock_grants(net_name)?;
        record_denied_locked(store, net_name, request, reason.clone(), now)?
    };
    journal_denied(store, net_name, &denied, reason);
    Ok(denied)
}

fn record_denied_locked(
    store: &Store,
    net_name: &str,
    mut request: AgentRequest,
    reason: DenyReason,
    now: u64,
) -> Result<AgentRequest> {
    request.state = RequestState::Denied {
        deny: reason,
        at: now,
    };
    request.updated_at = now;
    store.save_agent_request(net_name, &request)?;
    Ok(request)
}

/// The no-signature stop: written before the grant refund, so a crash
/// between the two leaves a `failed` request whose reservation a
/// reconciliation can still see on the grant.
fn record_failed_locked(
    store: &Store,
    net_name: &str,
    request: &mut AgentRequest,
    message: &str,
    now: u64,
) -> Result<()> {
    request.state = RequestState::Failed {
        message: message.to_string(),
        at: now,
    };
    request.updated_at = now;
    store.save_agent_request(net_name, request)?;
    journal_soft(
        store,
        net_name,
        request,
        EventKind::Failed {
            message: message.to_string(),
        },
    );
    Ok(())
}

fn record_unresolved_locked(
    store: &Store,
    net_name: &str,
    request: &mut AgentRequest,
    message: &str,
    txid: Option<String>,
    now: u64,
) -> Result<()> {
    request.state = RequestState::Unresolved {
        at: now,
        message: message.to_string(),
        txid,
    };
    request.updated_at = now;
    store.save_agent_request(net_name, request)?;
    journal_soft(
        store,
        net_name,
        request,
        EventKind::Failed {
            message: message.to_string(),
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use bdk_wallet::bitcoin::hashes::Hash;
    use bdk_wallet::bitcoin::{Amount, BlockHash, Psbt};
    use bdk_wallet::chain::{BlockId, ConfirmationBlockTime};
    use bdk_wallet::test_utils::{insert_checkpoint, receive_output};
    use sats_core::authz::{GRANT_FORMAT_VERSION, GrantMode};
    use sats_core::error::SignerError;
    use sats_core::plan::TransactionStatus;
    use sats_core::signer::LocalSigner;
    use sats_core::{seed, token};

    use super::*;
    use crate::config::Config;
    use crate::request::{CreateError, CreateParams, create, dismiss, list_reconciled};
    use crate::store::EventLine;

    const MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const ADDRESS: &str = "tb1pvlnw9n2zuefmxzwmuz0763uajw8nmaattkhd8002g3ekejjspxtshu2q9n";

    /// A store with a funded signet wallet and the file-driven mock
    /// provider: the same fixture the integration tests use, in-process.
    struct Fixture {
        dir: tempfile::TempDir,
        store: Store,
        mockdata: std::path::PathBuf,
    }

    impl Fixture {
        fn new(fund_sat: &[u64]) -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(Some(dir.path())).unwrap();
            let mockdata = dir.path().join("mockdata");
            std::fs::create_dir_all(&mockdata).unwrap();
            std::fs::write(mockdata.join("guard.json"), r#"{"protected": []}"#).unwrap();
            std::fs::write(
                dir.path().join("config.toml"),
                format!(
                    "network = \"signet\"\n\n[providers.mock]\ndriver = \"mock\"\nnetwork = \"signet\"\nurl = \"file://{}\"\n",
                    mockdata.display()
                ),
            )
            .unwrap();
            let mnemonic = seed::parse_mnemonic(MNEMONIC).unwrap();
            let (ext, int) = seed::public_descriptors(&mnemonic, Network::Signet).unwrap();
            walletd::create(&store, Network::Signet, ext, int).unwrap();
            {
                let mut ctx = walletd::open(&store, Network::Signet).unwrap();
                let block_900 = BlockId {
                    height: 900,
                    hash: BlockHash::all_zeros(),
                };
                insert_checkpoint(&mut ctx.wallet, block_900);
                insert_checkpoint(
                    &mut ctx.wallet,
                    BlockId {
                        height: 1_000,
                        hash: BlockHash::all_zeros(),
                    },
                );
                for value in fund_sat {
                    receive_output(
                        &mut ctx.wallet,
                        Amount::from_sat(*value),
                        ConfirmationBlockTime {
                            block_id: block_900,
                            confirmation_time: 100,
                        },
                    );
                }
                ctx.persist().unwrap();
            }
            Fixture {
                dir,
                store,
                mockdata,
            }
        }

        fn services(&self) -> Services {
            let config = Config::load(&self.store).unwrap();
            crate::provider::resolve(&config, &[], Network::Signet).unwrap()
        }

        /// Persist a signet grant for `agent` and hand back its token.
        fn grant_for(&self, agent: &str, budget_sat: u64, max_fee_sat: u64) -> String {
            let minted = token::generate().unwrap();
            let now = unix_now();
            let grant = Grant {
                format_version: GRANT_FORMAT_VERSION,
                agent: agent.into(),
                network: "signet".into(),
                budget_sat,
                spent_sat: 0,
                max_tx_sat: None,
                max_fee_sat,
                created_at: now,
                expires_at: now + 3_600,
                tx_count: 0,
                token_id: minted.token_id.clone(),
                token_hash: minted.token_hash.clone(),
                mode: GrantMode::Ask,
                allowed_recipients: None,
            };
            let _lock = self.store.lock_grants("signet").unwrap();
            self.store.save_grant("signet", &grant).unwrap();
            minted.secret.to_string()
        }

        fn grant(&self, budget_sat: u64, max_fee_sat: u64) -> String {
            self.grant_for("claude", budget_sat, max_fee_sat)
        }

        fn grant_state(&self) -> Grant {
            self.store.load_grant("signet", "claude").unwrap().unwrap()
        }

        fn revoke(&self) {
            let _lock = self.store.lock_grants("signet").unwrap();
            self.store.delete_grant("signet", "claude").unwrap();
        }

        fn request(&self, id: &str) -> AgentRequest {
            self.request_of("claude", id)
        }

        fn request_of(&self, agent: &str, id: &str) -> AgentRequest {
            self.store
                .load_agent_request("signet", agent, id)
                .unwrap()
                .unwrap()
        }

        fn create_as(
            &self,
            agent: &str,
            token: &str,
            key: &str,
            amount_sat: u64,
        ) -> Result<AgentRequest, CreateError> {
            create(
                &self.store,
                Network::Signet,
                &CreateParams {
                    agent,
                    token,
                    client_request_id: Some(key),
                    address: ADDRESS,
                    amount_sat,
                },
            )
        }

        fn create(&self, token: &str, key: &str, amount_sat: u64) -> AgentRequest {
            self.create_as("claude", token, key, amount_sat).unwrap()
        }

        fn events(&self) -> Vec<String> {
            self.store
                .list_event_lines("signet")
                .unwrap()
                .into_iter()
                .map(|line| match line {
                    EventLine::Event(event) => event.kind_str().to_string(),
                    EventLine::Unknown(_) => "unknown".to_string(),
                })
                .collect()
        }

        fn transactions(&self) -> usize {
            self.store.list_transactions("signet").unwrap().len()
        }
    }

    /// How the probe signer misbehaves, to reach each side of the
    /// signature boundary.
    #[derive(Clone, Copy, Default, PartialEq, Eq)]
    enum Fault {
        #[default]
        None,
        /// The signer reports an error: no signature was produced.
        Error,
        /// The signer signs but leaves the PSBT unfinalized.
        Unfinalized,
        /// The signer signs, then the PSBT no longer matches the prepared
        /// transaction, so finalization fails after the signature exists.
        Corrupt,
    }

    /// Both sides of the signer boundary: `builds` counts factory
    /// invocations — no denied or unauthorized request may even construct
    /// a signer — and `signs` counts signature attempts.
    #[derive(Clone, Default)]
    struct SignerProbe {
        builds: Rc<Cell<usize>>,
        signs: Rc<Cell<usize>>,
        fault: Fault,
    }

    impl SignerProbe {
        fn with(fault: Fault) -> SignerProbe {
            SignerProbe {
                fault,
                ..SignerProbe::default()
            }
        }

        fn counts(&self) -> (usize, usize) {
            (self.builds.get(), self.signs.get())
        }

        fn factory(&self) -> impl FnOnce() -> Result<Box<dyn Signer>> {
            let probe = self.clone();
            move || {
                probe.builds.set(probe.builds.get() + 1);
                let mnemonic = seed::parse_mnemonic(MNEMONIC).unwrap();
                Ok(Box::new(CountingSigner {
                    signs: probe.signs,
                    fault: probe.fault,
                    inner: LocalSigner::new(mnemonic, Network::Signet),
                }))
            }
        }
    }

    struct CountingSigner {
        signs: Rc<Cell<usize>>,
        fault: Fault,
        inner: LocalSigner,
    }

    impl Signer for CountingSigner {
        fn name(&self) -> &'static str {
            "counting"
        }

        fn sign(&mut self, psbt: &mut Psbt) -> Result<bool, SignerError> {
            self.signs.set(self.signs.get() + 1);
            match self.fault {
                Fault::None => self.inner.sign(psbt),
                Fault::Error => Err(SignerError::Unfinalized),
                Fault::Unfinalized => {
                    self.inner.sign(psbt)?;
                    Ok(false)
                }
                Fault::Corrupt => {
                    let finalized = self.inner.sign(psbt)?;
                    psbt.unsigned_tx.lock_time =
                        bdk_wallet::bitcoin::absolute::LockTime::from_consensus(7);
                    Ok(finalized)
                }
            }
        }
    }

    fn approve(fx: &Fixture, id: &str, probe: &SignerProbe) -> Result<Outcome> {
        let services = fx.services();
        match stage(&fx.store, Network::Signet, &services, id)? {
            Stage::Ready(staged) => commit(
                &fx.store,
                Network::Signet,
                &services,
                *staged,
                probe.factory(),
            ),
            Stage::Denied(_, reason) => Ok(Outcome::Denied(reason)),
        }
    }

    // ---- creation ------------------------------------------------------

    #[test]
    fn create_records_a_pending_request_bound_to_its_grant() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 1_000);
        let first = fx.create(&token, "job-1", 10_000);
        assert_eq!(first.id, "k-job-1");
        assert_eq!(first.state, RequestState::PendingApproval);
        assert_eq!(first.grant_token_id, fx.grant_state().token_id);
        assert_eq!(fx.events(), vec!["request_received"]);
        // Same key, same intent: the same record, no new event.
        let again = fx.create(&token, "job-1", 10_000);
        assert_eq!(again.id, first.id);
        assert_eq!(fx.events(), vec!["request_received"]);
        // Same key, different intent: a conflict, nothing written.
        let conflict = fx.create_as("claude", &token, "job-1", 20_000).unwrap_err();
        assert!(matches!(conflict, CreateError::Conflict { .. }));
        assert_eq!(fx.request("k-job-1").amount_sat, 10_000);
    }

    #[test]
    fn create_refuses_a_bad_token_and_writes_nothing() {
        let fx = Fixture::new(&[100_000]);
        let _token = fx.grant(50_000, 1_000);
        let err = fx
            .create_as("claude", "not-the-token", "job-1", 10_000)
            .unwrap_err();
        assert_eq!(err.code(), "unauthorized");
        assert!(
            fx.store
                .load_agent_request("signet", "claude", "k-job-1")
                .unwrap()
                .is_none()
        );
        assert!(fx.events().is_empty());
    }

    /// The audit line comes first: an unwritable log refuses the create
    /// and no request record exists without its causal event.
    #[test]
    fn create_refuses_when_the_audit_log_cannot_be_written() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 1_000);
        // A file where the events directory must be: every append fails.
        std::fs::write(fx.dir.path().join("signet/events"), b"in the way").unwrap();
        let err = fx.create_as("claude", &token, "job-1", 10_000).unwrap_err();
        assert_eq!(err.code(), "store_error");
        assert!(err.message("claude").contains("cannot record request"));
        assert!(
            fx.store
                .load_agent_request("signet", "claude", "k-job-1")
                .unwrap()
                .is_none(),
            "no record without its audit line"
        );
    }

    #[test]
    fn create_records_a_hard_boundary_as_denied() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(5_000, 1_000);
        let denied = fx.create(&token, "big", 10_000);
        assert!(matches!(
            denied.state,
            RequestState::Denied {
                deny: DenyReason::OverBudget { .. },
                ..
            }
        ));
        assert!(!denied.is_approvable());
        assert_eq!(fx.events(), vec!["request_received", "denied"]);
        // Approving a denied request is refused before anything runs.
        let probe = SignerProbe::default();
        let err = approve(&fx, "k-big", &probe).unwrap_err();
        assert!(err.to_string().contains("denied over_budget"), "{err:#}");
        assert_eq!(probe.counts(), (0, 0));
    }

    // ---- grant binding -------------------------------------------------

    /// A pending request outlives its grant: once the grant is revoked
    /// the request is non-executable and records why.
    #[test]
    fn revoked_grant_denies_the_pending_request() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 5_000);
        fx.create(&token, "pay", 10_000);
        fx.revoke();
        let probe = SignerProbe::default();
        let outcome = approve(&fx, "k-pay", &probe).unwrap();
        assert_eq!(outcome, Outcome::Denied(DenyReason::Revoked));
        assert_eq!(probe.counts(), (0, 0));
        let request = fx.request("k-pay");
        assert_eq!(request.state.deny_reason(), Some(&DenyReason::Revoked));
        assert!(!request.is_approvable());
        assert_eq!(fx.events(), vec!["request_received", "denied"]);
    }

    /// A re-issued grant for the same agent never inherits the old
    /// grant's requests: revoke, re-grant, approve still refuses.
    #[test]
    fn reissued_grant_does_not_execute_the_old_grants_request() {
        let fx = Fixture::new(&[100_000]);
        let old = fx.grant(50_000, 5_000);
        let request = fx.create(&old, "pay", 10_000);
        fx.revoke();
        let fresh = fx.grant(50_000, 5_000);
        assert_ne!(request.grant_token_id, fx.grant_state().token_id);
        let probe = SignerProbe::default();
        assert_eq!(
            approve(&fx, "k-pay", &probe).unwrap(),
            Outcome::Denied(DenyReason::Revoked)
        );
        assert_eq!(probe.counts(), (0, 0));
        assert_eq!(fx.grant_state().spent_sat, 0, "the new grant is untouched");
        assert_eq!(fx.request("k-pay").status(), "denied");
        // The old token cannot file under the new grant either.
        assert_eq!(
            fx.create_as("claude", &old, "pay-2", 10_000)
                .unwrap_err()
                .code(),
            "unauthorized"
        );
        // A request filed under the new grant executes normally.
        fx.create(&fresh, "pay-3", 10_000);
        assert!(matches!(
            approve(&fx, "k-pay-3", &probe).unwrap(),
            Outcome::Sent { .. }
        ));
        assert_eq!(probe.counts(), (1, 1));
    }

    /// Creation serializes with revoke and re-issue under the grant lock:
    /// a create that starts before a revoke either lands under the grant
    /// it authenticated against or fails, never under the replacement.
    #[test]
    fn concurrent_create_cannot_borrow_authority_across_a_reissue() {
        let fx = Fixture::new(&[100_000]);
        let old = fx.grant(50_000, 5_000);
        let old_id = fx.grant_state().token_id.clone();

        // Hold the grant lock, start the create, then revoke and re-grant
        // while the create is blocked on the lock.
        let lock = fx.store.lock_grants("signet").unwrap();
        let dir = fx.dir.path().to_path_buf();
        let old_token = old.clone();
        let worker = std::thread::spawn(move || {
            let store = Store::open(Some(&dir)).unwrap();
            create(
                &store,
                Network::Signet,
                &CreateParams {
                    agent: "claude",
                    token: &old_token,
                    client_request_id: Some("racy"),
                    address: ADDRESS,
                    amount_sat: 10_000,
                },
            )
            .map_err(|e| e.code())
        });
        // Give the worker time to reach the lock, then swap the grant
        // under it. delete/save go through the store, which the lock
        // serializes for every other writer.
        std::thread::sleep(std::time::Duration::from_millis(200));
        fx.store.delete_grant("signet", "claude").unwrap();
        let minted = token::generate().unwrap();
        let now = unix_now();
        fx.store
            .save_grant(
                "signet",
                &Grant {
                    format_version: GRANT_FORMAT_VERSION,
                    agent: "claude".into(),
                    network: "signet".into(),
                    budget_sat: 50_000,
                    spent_sat: 0,
                    max_tx_sat: None,
                    max_fee_sat: 5_000,
                    created_at: now,
                    expires_at: now + 3_600,
                    tx_count: 0,
                    token_id: minted.token_id.clone(),
                    token_hash: minted.token_hash.clone(),
                    mode: GrantMode::Ask,
                    allowed_recipients: None,
                },
            )
            .unwrap();
        drop(lock);

        // The create authenticated after the swap: the old token is dead.
        assert_eq!(worker.join().unwrap().unwrap_err(), "unauthorized");
        assert!(
            fx.store
                .load_agent_request("signet", "claude", "k-racy")
                .unwrap()
                .is_none(),
            "nothing was filed under either grant"
        );
        assert!(fx.events().is_empty());
        assert_ne!(fx.grant_state().token_id, old_id);
    }

    // ---- execution -----------------------------------------------------

    /// The whole loop: create → approve → sent, with exactly one signer
    /// construction and one signature.
    #[test]
    fn approve_executes_once_and_settles_to_sent() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 5_000);
        fx.create(&token, "pay", 10_000);
        let probe = SignerProbe::default();
        let outcome = approve(&fx, "k-pay", &probe).unwrap();
        let Outcome::Sent {
            txid,
            amount_sat,
            fee_sat,
            remaining_sat,
        } = outcome
        else {
            panic!("expected sent, got {outcome:?}");
        };
        assert_eq!(probe.counts(), (1, 1));
        assert_eq!(amount_sat, 10_000);
        assert!(fee_sat > 0);
        assert_eq!(remaining_sat, 50_000 - 10_000 - fee_sat);

        let request = fx.request("k-pay");
        assert_eq!(request.state.txid(), Some(txid.as_str()));
        assert_eq!(request.status(), "sent");
        let grant = fx.grant_state();
        assert_eq!(grant.spent_sat, 10_000 + fee_sat);
        assert_eq!(grant.tx_count, 1);
        let record = fx.store.load_transaction("signet", &txid).unwrap();
        assert_eq!(record.status, TransactionStatus::Broadcast);
        let origin = record.origin.as_ref().unwrap();
        assert_eq!(origin.agent.as_deref(), Some("claude"));
        assert_eq!(origin.request_id.as_deref(), Some("k-pay"));
        assert_eq!(
            origin.intent_digest.as_deref(),
            Some(request.intent_digest.as_str())
        );
        assert_eq!(
            fx.events(),
            vec![
                "request_received",
                "approved",
                "reserved",
                "signed",
                "broadcast"
            ]
        );
        assert!(
            std::fs::read_to_string(fx.mockdata.join("broadcasts.log"))
                .unwrap()
                .contains(&txid)
        );

        // A settled request cannot be approved or dismissed again: the
        // signer is never reached twice for one request.
        let again = approve(&fx, "k-pay", &probe).unwrap_err();
        assert!(again.to_string().contains("already sent"), "{again:#}");
        assert_eq!(probe.counts(), (1, 1));
        assert!(dismiss(&fx.store, Network::Signet, "k-pay").is_err());
    }

    /// The signer reports an error: no signature was produced, the
    /// reservation comes back, and the request is re-approvable.
    #[test]
    fn signer_error_refunds_and_leaves_the_request_re_approvable() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 5_000);
        fx.create(&token, "pay", 10_000);
        let failing = SignerProbe::with(Fault::Error);
        let outcome = approve(&fx, "k-pay", &failing).unwrap();
        assert!(matches!(outcome, Outcome::Failed { .. }), "{outcome:?}");
        assert_eq!(failing.counts(), (1, 1));
        let request = fx.request("k-pay");
        assert_eq!(request.status(), "failed");
        assert!(request.is_approvable());
        let grant = fx.grant_state();
        assert_eq!(grant.spent_sat, 0, "refunded");
        assert_eq!(grant.tx_count, 0);
        assert_eq!(fx.transactions(), 0);
        assert_eq!(
            fx.events(),
            vec![
                "request_received",
                "approved",
                "reserved",
                "failed",
                "refunded"
            ]
        );
        // A second authorization executes normally.
        let probe = SignerProbe::default();
        assert!(matches!(
            approve(&fx, "k-pay", &probe).unwrap(),
            Outcome::Sent { .. }
        ));
        assert_eq!(probe.counts(), (1, 1));
    }

    /// A signer that cannot be constructed never ran: refund.
    #[test]
    fn signer_construction_failure_refunds() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 5_000);
        fx.create(&token, "pay", 10_000);
        let services = fx.services();
        let Stage::Ready(staged) = stage(&fx.store, Network::Signet, &services, "k-pay").unwrap()
        else {
            panic!("expected ready");
        };
        let outcome = commit(&fx.store, Network::Signet, &services, *staged, || {
            bail!("no signer available")
        })
        .unwrap();
        assert!(matches!(outcome, Outcome::Failed { .. }), "{outcome:?}");
        assert_eq!(fx.request("k-pay").status(), "failed");
        assert_eq!(fx.grant_state().spent_sat, 0);
    }

    /// The signer signed but the transaction could not be finalized:
    /// a signature exists in memory, so nothing is refunded and the
    /// request is never signed again.
    #[test]
    fn finalization_failure_after_signing_is_unresolved_without_refund() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 5_000);
        fx.create(&token, "pay", 10_000);
        let probe = SignerProbe::with(Fault::Corrupt);
        let outcome = approve(&fx, "k-pay", &probe).unwrap();
        let Outcome::Unresolved { txid, .. } = outcome else {
            panic!("expected unresolved, got {outcome:?}");
        };
        assert!(txid.is_some(), "the unsigned txid is known");
        assert_eq!(probe.counts(), (1, 1));
        let request = fx.request("k-pay");
        assert_eq!(request.status(), "unresolved");
        assert!(request.state.may_have_signature());
        assert!(!request.is_approvable());
        assert!(fx.grant_state().spent_sat > 10_000, "never refunded");
        assert_eq!(fx.transactions(), 0);
        assert_eq!(
            fx.events(),
            vec!["request_received", "approved", "reserved", "failed"]
        );
        // Never a second signature, however the human asks.
        let again = approve(&fx, "k-pay", &probe).unwrap_err();
        assert!(again.to_string().contains("unresolved"), "{again:#}");
        assert_eq!(probe.counts(), (1, 1));
        list_reconciled(&fx.store, Network::Signet).unwrap();
        assert_eq!(fx.request("k-pay").status(), "unresolved");
        // The human closes it; the budget stays drawn.
        dismiss(&fx.store, Network::Signet, "k-pay").unwrap();
        assert_eq!(fx.request("k-pay").status(), "dismissed");
        assert!(fx.grant_state().spent_sat > 10_000);
    }

    /// The signer signed but left the PSBT unfinalized: a partial
    /// signature may exist. Same rule.
    #[test]
    fn unfinalized_signature_is_unresolved_without_refund() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 5_000);
        fx.create(&token, "pay", 10_000);
        let probe = SignerProbe::with(Fault::Unfinalized);
        let outcome = approve(&fx, "k-pay", &probe).unwrap();
        assert!(matches!(outcome, Outcome::Unresolved { .. }), "{outcome:?}");
        assert_eq!(fx.request("k-pay").status(), "unresolved");
        assert!(fx.grant_state().spent_sat > 10_000);
        assert!(approve(&fx, "k-pay", &probe).is_err());
        assert_eq!(probe.counts(), (1, 1));
    }

    /// The signature could not be persisted: it exists in memory, so no
    /// refund and no second signature.
    #[test]
    fn transaction_persistence_failure_after_signing_is_unresolved() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 5_000);
        fx.create(&token, "pay", 10_000);
        // A file where the transactions directory must be.
        std::fs::write(fx.dir.path().join("signet/transactions"), b"in the way").unwrap();
        let probe = SignerProbe::default();
        let outcome = approve(&fx, "k-pay", &probe).unwrap();
        let Outcome::Unresolved { txid, message } = outcome else {
            panic!("expected unresolved, got {outcome:?}");
        };
        assert!(message.contains("cannot save"), "{message}");
        assert!(txid.is_some());
        assert_eq!(probe.counts(), (1, 1));
        assert_eq!(fx.request("k-pay").status(), "unresolved");
        assert!(fx.grant_state().spent_sat > 10_000, "never refunded");
        assert!(approve(&fx, "k-pay", &probe).is_err());
        assert_eq!(probe.counts(), (1, 1), "never signs again");
    }

    /// Broadcast failure after signing: the signature is durable, the
    /// reservation stands, the request is `broadcast_pending`, and a
    /// second approve never signs again.
    #[test]
    fn broadcast_failure_is_broadcast_pending_and_never_signs_again() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 5_000);
        fx.create(&token, "pay", 10_000);
        std::fs::write(fx.mockdata.join("broadcast-fail"), "provider down").unwrap();
        let probe = SignerProbe::default();
        let outcome = approve(&fx, "k-pay", &probe).unwrap();
        let Outcome::BroadcastPending { txid, .. } = outcome else {
            panic!("expected broadcast_pending, got {outcome:?}");
        };
        assert_eq!(probe.counts(), (1, 1));
        let request = fx.request("k-pay");
        assert_eq!(request.status(), "broadcast_pending");
        assert!(request.state.has_signature());
        assert!(!request.is_approvable());
        let grant = fx.grant_state();
        assert!(grant.spent_sat > 10_000, "budget stays reserved");
        let record = fx.store.load_transaction("signet", &txid).unwrap();
        assert_eq!(record.status, TransactionStatus::Pending);
        assert_eq!(
            fx.events(),
            vec![
                "request_received",
                "approved",
                "reserved",
                "signed",
                "broadcast_failed"
            ]
        );
        let again = approve(&fx, "k-pay", &probe).unwrap_err();
        assert!(
            again.to_string().contains("signed but not broadcast"),
            "{again:#}"
        );
        assert_eq!(probe.counts(), (1, 1), "never signs again");

        // Rebroadcasting the existing transaction settles it.
        std::fs::remove_file(fx.mockdata.join("broadcast-fail")).unwrap();
        let services = fx.services();
        let mut ctx = walletd::open(&fx.store, Network::Signet).unwrap();
        let mut record = fx.store.load_transaction("signet", &txid).unwrap();
        crate::spend::broadcast_record(&fx.store, &mut ctx, &services, &mut record).unwrap();
        crate::request::settle_broadcast(&fx.store, Network::Signet, &txid).unwrap();
        assert_eq!(fx.request("k-pay").status(), "sent");
        assert_eq!(
            fx.grant_state().spent_sat,
            grant.spent_sat,
            "no second draw"
        );
    }

    // ---- crash recovery ------------------------------------------------

    /// Crash after `signing` reached disk, no transaction: the signer may
    /// have run. Recovery makes it `unresolved`, refunds nothing, and
    /// never signs again — repeatedly.
    #[test]
    fn crash_around_the_signer_is_unresolved_never_refunded_never_resigned() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 5_000);
        let mut request = fx.create(&token, "pay", 10_000);
        // Fabricate the crash: the grant drawn, the request signing.
        let mut grant = fx.grant_state();
        let spend = SpendRequest {
            amount_sat: 10_000,
            fee_sat: 300,
        };
        grant.reserve_send(ADDRESS, &spend, unix_now()).unwrap();
        fx.store.save_grant("signet", &grant).unwrap();
        request.state = RequestState::Signing {
            approved_at: unix_now(),
            fee_sat: 300,
        };
        fx.store.save_agent_request("signet", &request).unwrap();

        let settled = list_reconciled(&fx.store, Network::Signet).unwrap();
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].status(), "unresolved");
        assert!(!settled[0].is_approvable());
        let grant = fx.grant_state();
        assert_eq!(grant.spent_sat, 10_300, "never refunded");
        assert_eq!(grant.tx_count, 1);
        assert_eq!(fx.events(), vec!["request_received", "failed"]);
        // Reconciling again is a no-op, and approving refuses.
        list_reconciled(&fx.store, Network::Signet).unwrap();
        assert_eq!(fx.grant_state().spent_sat, 10_300);
        assert_eq!(fx.events().len(), 2);
        let probe = SignerProbe::default();
        assert!(approve(&fx, "k-pay", &probe).is_err());
        assert_eq!(probe.counts(), (0, 0));
    }

    /// Crash after the signature was persisted: the request becomes
    /// `broadcast_pending` (or `sent` if the record says so), with no
    /// refund.
    #[test]
    fn crash_after_persisted_signature_reconciles_without_refund() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 5_000);
        fx.create(&token, "pay", 10_000);
        // Run a real execution, then rewind the request to `signing`
        // as a crash between persisting the signature and settling would
        // leave it.
        std::fs::write(fx.mockdata.join("broadcast-fail"), "down").unwrap();
        let probe = SignerProbe::default();
        let Outcome::BroadcastPending { txid, fee_sat, .. } =
            approve(&fx, "k-pay", &probe).unwrap()
        else {
            panic!("expected broadcast_pending");
        };
        let spent = fx.grant_state().spent_sat;
        let mut request = fx.request("k-pay");
        request.state = RequestState::Signing {
            approved_at: unix_now(),
            fee_sat,
        };
        fx.store.save_agent_request("signet", &request).unwrap();

        let settled = list_reconciled(&fx.store, Network::Signet).unwrap();
        assert_eq!(settled[0].status(), "broadcast_pending");
        assert_eq!(settled[0].state.txid(), Some(txid.as_str()));
        assert_eq!(
            fx.grant_state().spent_sat,
            spent,
            "never refund a signature"
        );
        let again = approve(&fx, "k-pay", &probe).unwrap_err();
        assert!(again.to_string().contains("signed but not broadcast"));
        assert_eq!(probe.counts(), (1, 1));

        // The same record marked broadcast reconciles to `sent`.
        let mut tx_record = fx.store.load_transaction("signet", &txid).unwrap();
        tx_record.mark_broadcast();
        fx.store.save_transaction("signet", &tx_record).unwrap();
        request.state = RequestState::Signing {
            approved_at: unix_now(),
            fee_sat,
        };
        fx.store.save_agent_request("signet", &request).unwrap();
        let settled = list_reconciled(&fx.store, Network::Signet).unwrap();
        assert_eq!(settled[0].status(), "sent");
        assert_eq!(fx.grant_state().spent_sat, spent);
    }

    /// Request ids are agent-scoped: another agent's transaction with the
    /// same client id must never settle this agent's request.
    #[test]
    fn reconciliation_matches_the_full_attribution() {
        let fx = Fixture::new(&[100_000, 100_000]);
        let alice = fx.grant_for("alice", 50_000, 5_000);
        let bob = fx.grant_for("bob", 50_000, 5_000);
        fx.create_as("alice", &alice, "pay", 10_000).unwrap();
        fx.create_as("bob", &bob, "pay", 10_000).unwrap();
        // Alice's request executes and leaves a transaction attributed to
        // (alice, k-pay, alice's digest). Execute her by full id through
        // the executor's own lookup by making bob's record temporarily
        // unambiguous: approve alice first via a keyed stage on her id.
        let probe = SignerProbe::default();
        let services = fx.services();
        // `find_agent_request` resolves exact ids across agents and
        // refuses ambiguity, so drive alice's execution directly.
        let alice_request = fx.request_of("alice", "k-pay");
        let bob_request = fx.request_of("bob", "k-pay");
        assert_ne!(alice_request.intent_digest, bob_request.intent_digest);
        // Simulate alice's completed execution: a persisted transaction
        // attributed to her request.
        let mut ctx = walletd::open(&fx.store, Network::Signet).unwrap();
        let prepared = prepare::build(
            &mut ctx,
            &services,
            &prepare::PrepareRequest::for_agent(ADDRESS, 10_000),
        )
        .unwrap();
        ctx.persist().unwrap();
        let mut psbt = prepared.psbt().clone();
        let mut signer = probe.factory()().unwrap();
        assert!(signer.sign(&mut psbt).unwrap());
        let record = prepared
            .into_transaction(psbt)
            .unwrap()
            .with_origin(TxOrigin {
                surface: "agent".into(),
                agent: Some("alice".into()),
                request_id: Some("k-pay".into()),
                intent_digest: Some(alice_request.intent_digest.clone()),
            });
        fx.store.save_transaction("signet", &record).unwrap();

        // Bob's request was interrupted while signing. Alice's
        // transaction shares his request id; it must not settle him.
        let mut bob_signing = bob_request.clone();
        bob_signing.state = RequestState::Signing {
            approved_at: unix_now(),
            fee_sat: 300,
        };
        fx.store.save_agent_request("signet", &bob_signing).unwrap();
        list_reconciled(&fx.store, Network::Signet).unwrap();
        let bob_after = fx.request_of("bob", "k-pay");
        assert_eq!(
            bob_after.status(),
            "unresolved",
            "not settled by alice's tx"
        );
        assert_eq!(bob_after.state.txid(), None);

        // And alice's own interrupted record does settle from it.
        let mut alice_signing = alice_request.clone();
        alice_signing.state = RequestState::Signing {
            approved_at: unix_now(),
            fee_sat: 300,
        };
        fx.store
            .save_agent_request("signet", &alice_signing)
            .unwrap();
        list_reconciled(&fx.store, Network::Signet).unwrap();
        let alice_after = fx.request_of("alice", "k-pay");
        assert_eq!(alice_after.status(), "broadcast_pending");
        assert_eq!(alice_after.state.txid(), Some(record.txid.as_str()));
        // Settling the broadcast is attribution-checked the same way.
        let mut tx = fx.store.load_transaction("signet", &record.txid).unwrap();
        tx.mark_broadcast();
        fx.store.save_transaction("signet", &tx).unwrap();
        crate::request::settle_broadcast(&fx.store, Network::Signet, &record.txid).unwrap();
        assert_eq!(fx.request_of("alice", "k-pay").status(), "sent");
        assert_eq!(fx.request_of("bob", "k-pay").status(), "unresolved");
    }

    // ---- boundaries at execution --------------------------------------

    /// The real fee can cross a boundary the amount alone did not: the
    /// grant refuses at execution, nothing is drawn, nothing is signed.
    #[test]
    fn real_fee_over_the_cap_is_denied_at_execution() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 1);
        fx.create(&token, "pay", 10_000);
        let probe = SignerProbe::default();
        let outcome = approve(&fx, "k-pay", &probe).unwrap();
        assert!(
            matches!(outcome, Outcome::Denied(DenyReason::OverMaxFee { .. })),
            "{outcome:?}"
        );
        assert_eq!(probe.counts(), (0, 0));
        let request = fx.request("k-pay");
        assert_eq!(request.status(), "denied");
        assert!(!request.is_approvable());
        assert_eq!(fx.grant_state().spent_sat, 0);
        assert_eq!(fx.events(), vec!["request_received", "denied"]);
    }

    #[test]
    fn dismiss_moves_a_pending_request_and_refuses_settled_ones() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 5_000);
        fx.create(&token, "pay", 10_000);
        let dismissed = dismiss(&fx.store, Network::Signet, "k-pay").unwrap();
        assert_eq!(dismissed.status(), "dismissed");
        assert_eq!(fx.events(), vec!["request_received", "dismissed"]);
        assert!(dismiss(&fx.store, Network::Signet, "k-pay").is_err());
        let probe = SignerProbe::default();
        assert!(approve(&fx, "k-pay", &probe).is_err());
        assert_eq!(probe.counts(), (0, 0));
    }

    /// Stale sync on the human's side leaves the request untouched: the
    /// human retries when the chain is reachable.
    #[test]
    fn sync_failure_leaves_the_request_pending() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 5_000);
        fx.create(&token, "pay", 10_000);
        std::fs::write(fx.mockdata.join("sync-error"), "offline").unwrap();
        let probe = SignerProbe::default();
        let err = approve(&fx, "k-pay", &probe).unwrap_err();
        assert!(err.to_string().contains("stale state"), "{err:#}");
        assert_eq!(probe.counts(), (0, 0));
        assert_eq!(fx.request("k-pay").status(), "pending_approval");
        assert_eq!(fx.grant_state().spent_sat, 0);
        drop(fx.dir);
    }
}
