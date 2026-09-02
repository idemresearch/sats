//! Human-authorized execution of an agent request.
//!
//! Two phases, so a surface can put the real transaction in front of
//! the human before asking for their authorization:
//!
//! - [`stage`]: claim the request, prepare the transaction on the
//!   current chain state, derive what it actually pays from the wallet's
//!   own descriptors, and run the full ladder with the real fee. Nothing
//!   is reserved or signed; a policy refusal is recorded as `denied`.
//! - [`commit`]: reserve the budget and persist `executing` *before* any
//!   signature, sign through the signer factory (constructed only after
//!   the reservation is durable), persist the transaction before
//!   broadcasting, then settle to `sent` or `broadcast_pending`.
//!
//! The signature boundary is structural: `failed` means no durable
//! signature exists and the reservation was refunded; once a
//! transaction record is persisted the request is never signed again
//! and never refunded. This module is the only path by which an
//! agent-originated request reaches the signer, and it is invoked only
//! by a surface that has obtained the human's authorization.

use anyhow::{Context, Result, bail};
use sats_core::authz::{Decision, DenyReason, Grant, SpendRequest, evaluate_send};
use sats_core::bitcoin::Network;
use sats_core::event::EventKind;
use sats_core::plan::{PreparedSpend, TransactionRecord, TxOrigin};
use sats_core::request::{AgentRequest, RequestState};
use sats_core::signer::Signer;
use sats_core::verify;

use crate::commands::prepare;
use crate::config::network_name;
use crate::provider::Services;
use crate::request::{describe_settled, journal, journal_soft, reconcile};
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
    /// The grant refused it with the real fee; recorded as `denied`.
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
    /// Stopped before a durable signature existed; refunded.
    Failed { message: String },
    /// The grant refused it at the final re-check; nothing drawn.
    Denied(DenyReason),
}

/// Claim, prepare, verify, and re-check a request with its real fee.
///
/// The request must be `pending_approval` or `failed`; an interrupted
/// `executing` record is reconciled first. Preparation runs on the
/// human's side of the boundary with no safety bypasses. Operational
/// failures (sync, guards, fee estimation) leave the request untouched
/// so the human can try again; a mismatch between the prepared
/// transaction and the recorded intent refuses without signing.
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
                "request {} is executing right now — wait for it to settle",
                request.id
            )
        })?;
    let now = now_checked()?;
    let grant = store
        .load_grant(net_name, &request.agent)?
        .with_context(|| {
            format!(
                "no active grant for {:?} — issue a grant before approving",
                request.agent
            )
        })?;
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
/// and only after the reservation has been persisted, so no denied or
/// unauthorized request can ever construct a signer.
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

    // Reserve and persist BEFORE signing: once a signature exists the
    // money must be considered spent. The whole decision is atomic with
    // its persistence under the grant lock.
    let (record, remaining_sat) = {
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
        let mut grant = store
            .load_grant(net_name, &request.agent)?
            .with_context(|| format!("grant for {:?} was revoked", request.agent))?;
        if let Err(reason) = grant.reserve_send(&request.recipient, &spend, now) {
            let denied = record_denied_locked(store, net_name, request, reason.clone(), now)?;
            drop(grant_lock);
            journal_soft(
                store,
                net_name,
                &denied,
                EventKind::Denied {
                    deny: reason.clone(),
                    stage: "execute".into(),
                },
            );
            return Ok(Outcome::Denied(reason));
        }
        // Grant first, then the request: an `executing` record on disk
        // always means its draw is on the grant, so reconciliation can
        // refund it without guessing.
        store.save_grant(net_name, &grant)?;
        request.state = RequestState::Executing {
            approved_at: now,
            fee_sat: spend.fee_sat,
            grant_token_id: grant.token_id.clone(),
        };
        request.updated_at = now;
        store.save_agent_request(net_name, &request)?;
        journal_soft(store, net_name, &request, EventKind::Approved);
        if let Err(err) = journal(
            store,
            net_name,
            &request,
            EventKind::Reserved {
                total_sat: spend.total_sat(),
                remaining_sat: grant.remaining_sat(),
            },
        ) {
            // Nothing signed yet: refund and fail closed on a dead audit log.
            grant.refund(&spend);
            store.save_grant(net_name, &grant)?;
            let message = format!("cannot record reservation: {err:#}");
            record_failed_locked(store, net_name, &mut request, &message, now)?;
            return Ok(Outcome::Failed { message });
        }

        // Sign. The signer — and any key material inside it — is
        // constructed only here, after the reservation, and exists only
        // for this block.
        let signed = (|| -> Result<TransactionRecord> {
            let mut psbt = prepared.psbt().clone();
            let mut signer = make_signer()?;
            if !signer.sign(&mut psbt)? {
                bail!("signer produced an unfinalized transaction");
            }
            Ok(prepared.into_transaction(psbt)?)
        })();
        let record = match signed {
            Ok(record) => record.with_origin(TxOrigin {
                surface: "agent".into(),
                agent: Some(request.agent.clone()),
                request_id: Some(request.id.clone()),
                intent_digest: Some(request.intent_digest.clone()),
            }),
            Err(e) => {
                // No durable signature exists: refund the reservation.
                grant.refund(&spend);
                store.save_grant(net_name, &grant)?;
                let message = format!("signing failed: {e:#}");
                record_failed_locked(store, net_name, &mut request, &message, now)?;
                journal_soft(
                    store,
                    net_name,
                    &request,
                    EventKind::Refunded {
                        total_sat: spend.total_sat(),
                    },
                );
                return Ok(Outcome::Failed { message });
            }
        };
        // Persist the signature before it goes anywhere. Until this
        // write lands nothing durable exists; if it fails, the in-memory
        // signature dies with this process and the reservation returns.
        if let Err(e) = store.save_transaction(net_name, &record) {
            grant.refund(&spend);
            store.save_grant(net_name, &grant)?;
            let message = format!("cannot save signed transaction: {e:#}");
            record_failed_locked(store, net_name, &mut request, &message, now)?;
            journal_soft(
                store,
                net_name,
                &request,
                EventKind::Refunded {
                    total_sat: spend.total_sat(),
                },
            );
            return Ok(Outcome::Failed { message });
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
    journal_soft(
        store,
        net_name,
        &denied,
        EventKind::Denied {
            deny: reason,
            stage: "execute".into(),
        },
    );
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
    use crate::request::{CreateParams, create, dismiss, list_reconciled};
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

        /// Persist a signet grant for "claude" and hand back its token.
        fn grant(&self, budget_sat: u64, max_fee_sat: u64) -> String {
            let minted = token::generate().unwrap();
            let now = unix_now();
            let grant = Grant {
                format_version: GRANT_FORMAT_VERSION,
                agent: "claude".into(),
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
            self.store.save_grant("signet", &grant).unwrap();
            minted.secret.to_string()
        }

        fn grant_state(&self) -> Grant {
            self.store.load_grant("signet", "claude").unwrap().unwrap()
        }

        fn request(&self, id: &str) -> AgentRequest {
            self.store
                .load_agent_request("signet", "claude", id)
                .unwrap()
                .unwrap()
        }

        fn create(&self, token: &str, key: &str, amount_sat: u64) -> AgentRequest {
            create(
                &self.store,
                Network::Signet,
                &CreateParams {
                    agent: "claude",
                    token,
                    client_request_id: Some(key),
                    address: ADDRESS,
                    amount_sat,
                },
            )
            .unwrap()
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
    }

    /// Both sides of the signer boundary: `builds` counts factory
    /// invocations — no denied or unauthorized request may even construct
    /// a signer — and `signs` counts signature attempts.
    #[derive(Clone, Default)]
    struct SignerProbe {
        builds: Rc<Cell<usize>>,
        signs: Rc<Cell<usize>>,
        fail: bool,
    }

    impl SignerProbe {
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
                    fail: probe.fail,
                    inner: LocalSigner::new(mnemonic, Network::Signet),
                }))
            }
        }
    }

    struct CountingSigner {
        signs: Rc<Cell<usize>>,
        fail: bool,
        inner: LocalSigner,
    }

    impl Signer for CountingSigner {
        fn name(&self) -> &'static str {
            "counting"
        }

        fn sign(&mut self, psbt: &mut Psbt) -> Result<bool, SignerError> {
            self.signs.set(self.signs.get() + 1);
            if self.fail {
                return Err(SignerError::Unfinalized);
            }
            self.inner.sign(psbt)
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

    #[test]
    fn create_records_a_pending_request_and_repeats_idempotently() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 1_000);
        let first = fx.create(&token, "job-1", 10_000);
        assert_eq!(first.id, "k-job-1");
        assert_eq!(first.state, RequestState::PendingApproval);
        assert_eq!(fx.events(), vec!["request_received"]);
        // Same key, same intent: the same record, no new event.
        let again = fx.create(&token, "job-1", 10_000);
        assert_eq!(again.id, first.id);
        assert_eq!(fx.events(), vec!["request_received"]);
        // Same key, different intent: a conflict, nothing written.
        let conflict = create(
            &fx.store,
            Network::Signet,
            &CreateParams {
                agent: "claude",
                token: &token,
                client_request_id: Some("job-1"),
                address: ADDRESS,
                amount_sat: 20_000,
            },
        )
        .unwrap_err();
        assert!(matches!(
            conflict,
            super::super::CreateError::Conflict { .. }
        ));
        assert_eq!(fx.request("k-job-1").amount_sat, 10_000);
    }

    #[test]
    fn create_refuses_a_bad_token_and_writes_nothing() {
        let fx = Fixture::new(&[100_000]);
        let _token = fx.grant(50_000, 1_000);
        let err = create(
            &fx.store,
            Network::Signet,
            &CreateParams {
                agent: "claude",
                token: "not-the-token",
                client_request_id: Some("job-1"),
                address: ADDRESS,
                amount_sat: 10_000,
            },
        )
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
        assert_eq!(
            request.state.txid(),
            Some(txid.as_str()),
            "the request carries the txid"
        );
        assert_eq!(request.status(), "sent");
        let grant = fx.grant_state();
        assert_eq!(grant.spent_sat, 10_000 + fee_sat);
        assert_eq!(grant.tx_count, 1);
        let record = fx.store.load_transaction("signet", &txid).unwrap();
        assert_eq!(record.status, TransactionStatus::Broadcast);
        assert_eq!(
            record.origin.as_ref().and_then(|o| o.request_id.as_deref()),
            Some("k-pay")
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

    /// Signing failure: nothing durable exists, the reservation comes
    /// back, and the request is re-approvable.
    #[test]
    fn signing_failure_refunds_and_leaves_the_request_re_approvable() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 5_000);
        fx.create(&token, "pay", 10_000);
        let failing = SignerProbe {
            fail: true,
            ..SignerProbe::default()
        };
        let outcome = approve(&fx, "k-pay", &failing).unwrap();
        assert!(matches!(outcome, Outcome::Failed { .. }), "{outcome:?}");
        assert_eq!(failing.counts(), (1, 1));
        let request = fx.request("k-pay");
        assert_eq!(request.status(), "failed");
        assert!(request.is_approvable());
        let grant = fx.grant_state();
        assert_eq!(grant.spent_sat, 0, "refunded");
        assert_eq!(grant.tx_count, 0);
        assert!(fx.store.list_transactions("signet").unwrap().is_empty());
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

    /// Crash before a durable signature: an `executing` record with no
    /// transaction reconciles to `failed` and refunds exactly once.
    #[test]
    fn interrupted_before_signature_refunds_and_fails() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 5_000);
        let mut request = fx.create(&token, "pay", 10_000);
        // Fabricate the crash: the grant drawn, the request executing.
        let mut grant = fx.grant_state();
        let spend = SpendRequest {
            amount_sat: 10_000,
            fee_sat: 300,
        };
        grant.reserve_send(ADDRESS, &spend, unix_now()).unwrap();
        fx.store.save_grant("signet", &grant).unwrap();
        request.state = RequestState::Executing {
            approved_at: unix_now(),
            fee_sat: 300,
            grant_token_id: grant.token_id.clone(),
        };
        fx.store.save_agent_request("signet", &request).unwrap();

        let settled = list_reconciled(&fx.store, Network::Signet).unwrap();
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].status(), "failed");
        assert!(settled[0].is_approvable());
        let grant = fx.grant_state();
        assert_eq!(grant.spent_sat, 0, "refunded exactly the reservation");
        assert_eq!(grant.tx_count, 0);
        assert_eq!(fx.events(), vec!["request_received", "refunded", "failed"]);
        // Reconciling again is a no-op: no double refund.
        list_reconciled(&fx.store, Network::Signet).unwrap();
        assert_eq!(fx.grant_state().spent_sat, 0);
        assert_eq!(fx.events().len(), 3);
    }

    /// A replaced grant never inherits a refund it did not draw.
    #[test]
    fn interrupted_execution_does_not_refund_a_replaced_grant() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 5_000);
        let mut request = fx.create(&token, "pay", 10_000);
        request.state = RequestState::Executing {
            approved_at: unix_now(),
            fee_sat: 300,
            grant_token_id: "old-grant".into(),
        };
        fx.store.save_agent_request("signet", &request).unwrap();
        let settled = list_reconciled(&fx.store, Network::Signet).unwrap();
        assert_eq!(settled[0].status(), "failed");
        assert_eq!(fx.grant_state().spent_sat, 0);
        assert_eq!(fx.events(), vec!["request_received", "failed"]);
    }

    /// Crash after the signature was persisted: never refund; the request
    /// becomes `broadcast_pending` (or `sent` if the record says so).
    #[test]
    fn interrupted_after_signature_reconciles_without_refund() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 5_000);
        fx.create(&token, "pay", 10_000);
        // Run a real execution, then rewind the request to `executing`
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
        request.state = RequestState::Executing {
            approved_at: unix_now(),
            fee_sat,
            grant_token_id: fx.grant_state().token_id.clone(),
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
        request.state = RequestState::Executing {
            approved_at: unix_now(),
            fee_sat,
            grant_token_id: fx.grant_state().token_id.clone(),
        };
        fx.store.save_agent_request("signet", &request).unwrap();
        let settled = list_reconciled(&fx.store, Network::Signet).unwrap();
        assert_eq!(settled[0].status(), "sent");
        assert_eq!(fx.grant_state().spent_sat, spent);
    }

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

    /// Revocation between creation and approval: nothing executes.
    #[test]
    fn revoked_grant_refuses_to_execute() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant(50_000, 5_000);
        fx.create(&token, "pay", 10_000);
        fx.store.delete_grant("signet", "claude").unwrap();
        let probe = SignerProbe::default();
        let err = approve(&fx, "k-pay", &probe).unwrap_err();
        assert!(err.to_string().contains("no active grant"), "{err:#}");
        assert_eq!(probe.counts(), (0, 0));
        assert_eq!(fx.request("k-pay").status(), "pending_approval");
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
