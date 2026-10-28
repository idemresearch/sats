//! Human-authorized execution of an agent request.
//!
//! Two phases, so a surface can put the real transaction in front of
//! the human before asking for their authorization:
//!
//! - [`stage`]: claim the request, prepare the transaction on the
//!   current chain state, derive what it actually pays from the wallet's
//!   own descriptors, and re-run the grant's ladder on that amount.
//!   Nothing is signed; a policy refusal is recorded as `denied`.
//! - [`commit`]: re-check the request and its grant under the grant lock
//!   and persist `signing` *before* the signer is invoked, sign through
//!   the signer factory (constructed only after `signing` is durable),
//!   persist the transaction before broadcasting, then settle to `sent`
//!   or `broadcast_pending`.
//!
//! The irreversible boundary is the invocation of `Signer::sign`. Before
//! it — the audit append, the signer's construction — a failure leaves
//! the request `failed`, re-approvable, because this process knows no
//! signature exists. From the invocation on, nothing the signer reports
//! is trusted to mean "no signature": an error, an unfinalized result, a
//! finalization or persistence failure, and a crash all leave the
//! request `unresolved` — never signed again by sats. A human resolves
//! it.
//!
//! A request executes only under the grant instance that created it.
//! This module is the only path by which an agent-originated request
//! reaches the signer, and it is invoked only by a surface that has
//! obtained the human's authorization.

use crate::alkanes::{
    transfer::{self, TransferRequest, TransferReview},
    wrapping::{self, WrapReview},
};
use anyhow::{Context, Result, bail};
use sats_core::authz::{Decision, DenyReason, Grant, SpendRequest, evaluate_send};
use sats_core::bitcoin::Network;
use sats_core::event::EventKind;
use sats_core::intent::RequestOperation;
use sats_core::plan::{PreparedSpend, TxOrigin};
use sats_core::request::{AgentRequest, RequestState};
use sats_core::signer::Signer;
use sats_core::verify;

use crate::config::network_name;
use crate::prepare;
use crate::provider::Services;
use crate::request::{bound_grant, describe_settled, journal, journal_soft, reconcile};
use crate::store::{RequestClaim, Store, now_checked, unix_now};
use crate::walletd::{self, WalletCtx};

/// A request prepared on current chain state and verified against its
/// recorded intent, awaiting the human's authorization to execute.
///
/// Read-only outside this module: [`commit`] signs exactly what
/// [`stage`] verified, so a surface can show these values but never
/// change what gets signed or which request is executed.
///
/// ```
/// fn review(staged: &sats_wallet::request::execute::Staged) -> u64 {
///     staged.spend().total_sat()
/// }
/// ```
///
/// ```compile_fail
/// fn shrink(staged: &mut sats_wallet::request::execute::Staged) {
///     staged.spend.amount_sat = 1;
/// }
/// ```
pub struct Staged {
    request: AgentRequest,
    grant: Grant,
    spend: SpendRequest,
    prepared: PreparedSpend,
    ctx: WalletCtx,
    /// Resolved after the local prechecks; the same selection serves broadcast.
    services: Services,
    wrap_review: Option<WrapReview>,
    asset_transfer_review: Option<TransferReview>,
    _claim: RequestClaim,
}

impl Staged {
    /// The request being approved, as claimed at staging.
    pub fn request(&self) -> &AgentRequest {
        &self.request
    }

    /// The grant as it stood at staging, for display. The commit re-reads
    /// it under the lock.
    pub fn grant(&self) -> &Grant {
        &self.grant
    }

    /// The additional public facts a human reviews for a wrapping request.
    pub fn wrap_review(&self) -> Option<&WrapReview> {
        self.wrap_review.as_ref()
    }

    /// Indexed asset effects bound to this exact reviewed transaction.
    pub fn asset_transfer_review(&self) -> Option<&TransferReview> {
        self.asset_transfer_review.as_ref()
    }

    /// What the prepared transaction pays and costs, derived from the
    /// PSBT, not from the caller.
    pub fn spend(&self) -> &SpendRequest {
        &self.spend
    }
}

/// What staging resolved to.
pub enum Stage {
    /// Prepared and inside the grant: ready to commit.
    Ready(Box<Staged>),
    /// The grant refused it, or the grant that created it is gone;
    /// recorded as `denied`.
    Denied(Box<AgentRequest>, DenyReason),
}

/// How an execution ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Sent {
        txid: String,
        amount_sat: u64,
        fee_sat: u64,
    },
    /// Signed and persisted; the broadcast failed. The signer is never
    /// invoked again; `sats tx broadcast <txid>` retries.
    BroadcastPending {
        txid: String,
        amount_sat: u64,
        fee_sat: u64,
        message: String,
    },
    /// The signer was invoked and the outcome could not be made durable:
    /// a signature may exist. Never signed again.
    Unresolved {
        txid: Option<String>,
        message: String,
    },
    /// Stopped before the signer was invoked; re-approvable.
    Failed { message: String },
    /// The grant refused it at the final re-check; nothing signed.
    Denied(DenyReason),
}

/// Claim, prepare, verify, and re-check a request against its grant.
///
/// The request must be `pending_approval` or `failed`; an interrupted
/// `signing` record is reconciled first. The grant on file must be the
/// instance that created the request. Providers are resolved only after
/// these local checks pass. Preparation runs on the human's
/// side of the boundary with no safety bypasses. Operational failures
/// (sync, fee estimation) record a safe pre-signature failure so
/// the human can inspect the attempt and authorize again; a mismatch between the prepared transaction and
/// the recorded intent refuses without signing.
pub fn stage(
    store: &Store,
    network: Network,
    resolve_services: impl FnOnce() -> Result<Services>,
    id_or_prefix: &str,
) -> Result<Stage> {
    let net_name = network_name(network);
    let request = store.find_agent_request(net_name, id_or_prefix)?;
    let request = reconcile(store, network, request)?;
    if request.expected_digest() != request.intent_digest {
        bail!("request facts do not match its intent digest; refusing execution");
    }
    if let RequestOperation::FrbtcWrap { contract } = &request.operation
        && contract.signer_address != request.recipient
    {
        bail!("frBTC signer does not match the request recipient; refusing execution");
    }
    if let RequestOperation::AssetTransfer { transfer } = &request.operation {
        if transfer.asset.protocol != "alkanes" {
            bail!("unsupported asset protocol; refusing execution");
        }
        let id = transfer
            .asset
            .id
            .parse::<sats_alkanes::id::AlkaneId>()
            .map_err(|e| anyhow::anyhow!(e))?;
        if id.to_string() != transfer.asset.id
            || transfer.amount == 0
            || request.amount_sat != sats_alkanes::build::DEFAULT_POSTAGE_SAT
        {
            bail!("invalid asset transfer facts; refusing execution");
        }
    }
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
    // Precheck on the recorded amount, before spending a chain sync on a
    // request the grant now refuses.
    if let Decision::Deny(reason) =
        evaluate_send(&grant, &request.recipient, request.amount_sat, now)
    {
        let denied = record_denied(store, net_name, request, reason.clone(), now)?;
        return Ok(Stage::Denied(Box::new(denied), reason));
    }

    // Preparation: the shared pipeline, agent form — no dust bypass, and
    // a failed sync is a hard stop.
    let preparation = (|| -> Result<_> {
        let services = resolve_services()?;
        match &request.operation {
            RequestOperation::Send => {
                let mut ctx = walletd::open(store, network)?;
                let prepare_request =
                    prepare::PrepareRequest::for_agent(&request.recipient, request.amount_sat);
                let prepared = prepare::build(&mut ctx, &services, &prepare_request)?;
                ctx.persist()?;
                Ok((ctx, services, prepared, None, None))
            }
            RequestOperation::FrbtcWrap { contract } => {
                let wrap =
                    wrapping::prepare(store, network, &services, contract, request.amount_sat)?;
                Ok((
                    wrap.ctx,
                    services,
                    wrap.prepared,
                    Some((wrap.spend, wrap.review)),
                    None,
                ))
            }
            RequestOperation::AssetTransfer { transfer: intent } => {
                let args = TransferRequest {
                    asset: intent
                        .asset
                        .id
                        .parse()
                        .map_err(|e| anyhow::anyhow!("{e}"))?,
                    amount: intent.amount,
                    address: request.recipient.clone(),
                    fee_rate: None,
                    postage_sat: request.amount_sat,
                };
                let transfer = transfer::prepare(store, network, &services, &args)?;
                if transfer.review.asset.to_string() != intent.asset.id
                    || transfer.review.amount != intent.amount
                    || transfer.review.recipient != request.recipient
                    || transfer.review.summary.recipient_sat != request.amount_sat
                {
                    bail!("prepared asset transfer disagrees with the request");
                }
                Ok((
                    transfer.ctx,
                    services,
                    transfer.prepared,
                    None,
                    Some(transfer.review),
                ))
            }
        }
    })();
    let (ctx, services, prepared, wrap_stage, asset_transfer_review) = match preparation {
        Ok(prepared) => prepared,
        Err(err) => {
            // The claim excludes another execution, but a human may dismiss
            // while provider I/O runs. Re-read under the grant lock and never
            // replace a settled or potentially signed state with a retry.
            let recorded = record_preparation_failure(store, net_name, &request, &err);
            return match recorded {
                Ok(()) => Err(err),
                Err(write_err) => Err(err.context(format!(
                    "preparation failed; cannot record the attempt: {write_err:#}"
                ))),
            };
        }
    };

    // What this transaction actually does, per our own descriptors. The
    // recorded intent committed to one exact payment; a PSBT that pays
    // anything else is not this request.
    let (spend, wrap_review) = if let Some((spend, review)) = wrap_stage {
        (spend, Some(review))
    } else if let Some(review) = &asset_transfer_review {
        (
            SpendRequest {
                amount_sat: review.summary.recipient_sat,
                fee_sat: review.summary.fee_sat,
            },
            None,
        )
    } else {
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

        (spend, None)
    };

    // The ladder again on what the transaction actually pays, never on
    // the recorded figure alone. A refusal is recorded, terminal.
    if let Decision::Deny(reason) = evaluate_send(&grant, &request.recipient, spend.amount_sat, now)
    {
        let denied = record_denied(store, net_name, request, reason.clone(), now)?;
        return Ok(Stage::Denied(Box::new(denied), reason));
    }

    Ok(Stage::Ready(Box::new(Staged {
        request,
        grant,
        spend,
        prepared,
        ctx,
        services,
        wrap_review,
        asset_transfer_review,
        _claim: claim,
    })))
}

/// Execute a staged request with the human's authorization.
///
/// `make_signer` is the signer boundary: it is invoked exactly once,
/// and only after the `signing` record has been persisted, so no denied
/// or unauthorized request can ever construct a
/// signer, and a crash at any later point is recoverable as `unresolved`.
pub fn commit(
    store: &Store,
    network: Network,
    staged: Staged,
    make_signer: impl FnOnce() -> Result<Box<dyn Signer>>,
) -> Result<Outcome> {
    commit_inner(store, network, staged, make_signer, &|_| Ok(()))
}

/// [`commit`] with a crash seam. `crash` is consulted at each named
/// point between two durable writes; an error from it aborts exactly as
/// a dead process would, with nothing written afterwards. Production
/// passes a hook that never fires; the crash-window tests fire it.
fn commit_inner(
    store: &Store,
    network: Network,
    staged: Staged,
    make_signer: impl FnOnce() -> Result<Box<dyn Signer>>,
    crash: &dyn Fn(&str) -> Result<()>,
) -> Result<Outcome> {
    let net_name = network_name(network);
    let Staged {
        mut request,
        spend,
        prepared,
        mut ctx,
        services,
        asset_transfer_review,
        _claim,
        ..
    } = staged;

    let record = {
        // The whole decision is atomic with its persistence under the
        // grant lock: re-read the request and the grant, re-check, persist.
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
        if !fresh.version_supported()
            || fresh.expected_digest() != fresh.intent_digest
            || fresh.intent_digest != request.intent_digest
            || fresh.grant_id != request.grant_id
            || fresh.agent != request.agent
            || fresh.network != request.network
            || fresh.recipient != request.recipient
            || fresh.amount_sat != request.amount_sat
            || fresh.operation != request.operation
        {
            bail!("request facts changed after review; refusing execution");
        }
        request = fresh;
        let grant = match bound_grant(store, net_name, &request)? {
            Ok(grant) => grant,
            Err(reason) => {
                let denied = record_denied_locked(store, net_name, request, reason.clone(), now)?;
                drop(grant_lock);
                journal_denied(store, net_name, &denied, reason.clone());
                return Ok(Outcome::Denied(reason));
            }
        };
        if let Decision::Deny(reason) =
            evaluate_send(&grant, &request.recipient, spend.amount_sat, now)
        {
            let denied = record_denied_locked(store, net_name, request, reason.clone(), now)?;
            drop(grant_lock);
            journal_denied(store, net_name, &denied, reason.clone());
            return Ok(Outcome::Denied(reason));
        }
        if let RequestOperation::FrbtcWrap { contract } = &request.operation
            && let Err(err) = wrapping::check_contract(&services, contract)
        {
            let message = format!("cannot re-verify frBTC contract: {err:#}");
            record_failed_locked(store, net_name, &mut request, &message, now)?;
            return Ok(Outcome::Failed { message });
        }
        if let Some(review) = &asset_transfer_review
            && let Err(err) = transfer::recheck(&ctx, &services, prepared.psbt(), review)
        {
            let message = format!("cannot re-verify asset transfer: {err:#}");
            record_failed_locked(store, net_name, &mut request, &message, now)?;
            return Ok(Outcome::Failed { message });
        }
        // Provider rechecks can take time: expiry must still hold when
        // `signing` is persisted.
        let now = now_checked()?;
        if let Decision::Deny(reason) =
            evaluate_send(&grant, &request.recipient, spend.amount_sat, now)
        {
            let denied = record_denied_locked(store, net_name, request, reason.clone(), now)?;
            drop(grant_lock);
            journal_denied(store, net_name, &denied, reason.clone());
            return Ok(Outcome::Denied(reason));
        }

        // Persist `signing` BEFORE signing: on disk it is the irreversible
        // boundary, so a crash from here on can never be mistaken for a
        // request that provably never reached the signer.
        request.state = RequestState::Signing {
            approved_at: now,
            fee_sat: spend.fee_sat,
        };
        request.updated_at = now;
        store.save_agent_request(net_name, &request)?;
        crash("signing-saved")?;

        // Everything from here to the signer invocation is still on the
        // near side of the boundary: this process knows the signer has
        // not run, so a failure leaves the request `failed` and
        // re-approvable. On disk the request already says `signing`,
        // which is what a crash in this window must look like.
        let before_signer = |request: &mut AgentRequest, message: String| -> Result<Outcome> {
            record_failed_locked(store, net_name, request, &message, now)?;
            Ok(Outcome::Failed { message })
        };
        if let Err(err) = journal(store, net_name, &request, EventKind::Approved) {
            // Fail closed on a dead audit log, before the signer exists.
            let message = format!("cannot record the approval: {err:#}");
            return before_signer(&mut request, message);
        }
        let mut signer = match make_signer() {
            Ok(signer) => signer,
            Err(err) => {
                let message = format!("cannot construct the signer: {err:#}");
                return before_signer(&mut request, message);
            }
        };

        // ---- the irreversible boundary ----
        // The signer is invoked. Nothing it reports afterwards is trusted
        // to mean "no signature": a signer may have signed, or leaked a
        // signature, before returning an error.
        let mut psbt = prepared.psbt().clone();
        let unsigned_txid = psbt.unsigned_tx.compute_txid().to_string();
        let signed = signer.sign(&mut psbt);
        drop(signer);
        let unresolved = |request: &mut AgentRequest, message: String| -> Result<Outcome> {
            // A signature may exist: this request is never signed again
            // by sats.
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
        let finalized = match signed {
            Ok(finalized) => finalized,
            Err(err) => {
                let message =
                    format!("signer failed after it was invoked: {err} — a signature may exist");
                return unresolved(&mut request, message);
            }
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
        record
    };

    // Broadcast outside the lock: a slow provider must not stall the
    // wallet. A failure never signs again.
    let mut record = record;
    match crate::spend::broadcast_record(store, &mut ctx, &services, &mut record) {
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
            })
        }
        Err(err) => {
            let txid = record.txid.clone();
            let message = format!(
                "broadcast failed after signing: {err:#} — the signed transaction is saved; \
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
                log::warn!("request record not updated: {err:#}");
            }
        }
        Err(err) => log::warn!("request record not updated: {err:#}"),
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

/// Record only a provable pre-signing attempt. The caller holds the
/// execution claim; the grant lock protects the fresh state check/write.
fn record_preparation_failure(
    store: &Store,
    net_name: &str,
    request: &AgentRequest,
    error: &anyhow::Error,
) -> Result<()> {
    let _lock = store.lock_grants(net_name)?;
    let mut fresh = store
        .load_agent_request(net_name, &request.agent, &request.id)?
        .context("request disappeared during preparation")?;
    if !fresh.version_supported()
        || fresh.id != request.id
        || fresh.agent != request.agent
        || fresh.network != request.network
        || fresh.grant_id != request.grant_id
        || fresh.intent_digest != request.intent_digest
        || fresh.recipient != request.recipient
        || fresh.amount_sat != request.amount_sat
        || fresh.operation != request.operation
    {
        bail!("request changed during preparation; refusing to overwrite the record");
    }
    if !fresh.is_approvable() {
        return Ok(());
    }
    let message = format!("preparation failed before signing: {error:#}");
    record_failed_locked(store, net_name, &mut fresh, &message, unix_now())
}

/// The pre-invocation stop: this process knows the signer never ran, so
/// the request stays re-approvable.
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
        txid: txid.clone(),
    };
    request.updated_at = now;
    store.save_agent_request(net_name, request)?;
    journal_soft(
        store,
        net_name,
        request,
        EventKind::Unresolved {
            message: message.to_string(),
            txid,
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
    use sats_core::authz::{GRANT_FORMAT_VERSION, GrantMode, new_grant_id};
    use sats_core::error::SignerError;
    use sats_core::plan::TransactionStatus;
    use sats_core::signer::LocalSigner;
    use sats_core::{seed, token};

    use super::*;
    use crate::config::Config;
    use crate::request::{
        CreateError, CreateParams, create, dismiss, is_request_id, keyed_request_id,
        list_reconciled,
    };
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
            let store = Store::open(dir.path());
            let mockdata = dir.path().join("mockdata");
            std::fs::create_dir_all(&mockdata).unwrap();
            std::fs::write(mockdata.join("alkanes-outpoints.json"), "{}").unwrap();
            std::fs::write(
                dir.path().join("config.toml"),
                format!(
                    "network = \"signet\"\n\n[signet]\nchain = \"esplora\"\nsubfrost_url = \"file://{0}\"\n\n[signet.esplora]\nurl = \"file://{0}\"\n",
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
            crate::provider::resolve(&config, None, Network::Signet).unwrap()
        }

        /// Persist a signet grant for `agent` and hand back its token.
        fn grant_for(&self, agent: &str) -> String {
            let minted = token::generate().unwrap();
            let grant = fresh_grant(agent, &minted);
            let _lock = self.store.lock_grants("signet").unwrap();
            self.store.save_grant("signet", &grant).unwrap();
            minted.secret.to_string()
        }

        fn grant(&self) -> String {
            self.grant_for("claude")
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
                    idempotency_key: key,
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

        fn count_events(&self, kind: &str) -> usize {
            self.events().iter().filter(|k| k.as_str() == kind).count()
        }

        fn transactions(&self) -> usize {
            self.store.list_transactions("signet").unwrap().len()
        }
    }

    fn fresh_grant(agent: &str, minted: &token::NewToken) -> Grant {
        let now = unix_now();
        Grant {
            format_version: GRANT_FORMAT_VERSION,
            agent: agent.into(),
            network: "signet".into(),
            max_tx_sat: None,
            created_at: now,
            expires_at: now + 3_600,
            token_id: minted.token_id.clone(),
            token_hash: minted.token_hash.clone(),
            mode: GrantMode::Ask,
            allowed_recipients: None,
            grant_id: new_grant_id().unwrap(),
        }
    }

    /// How the probe signer misbehaves, to reach each side of the
    /// signature boundary.
    #[derive(Clone, Copy, Default, PartialEq, Eq)]
    enum Fault {
        #[default]
        None,
        /// The signer reports an error without touching the PSBT.
        Error,
        /// The malicious case: the signer signs the PSBT, then reports an
        /// error anyway. A signature exists despite the error.
        SignThenError,
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
                    inner: LocalSigner::new(mnemonic, Network::Signet).unwrap(),
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
                Fault::SignThenError => {
                    let finalized = self.inner.sign(psbt)?;
                    assert!(finalized, "the malicious signer really signed");
                    Err(SignerError::Unfinalized)
                }
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
        approve_crashing_at(fx, id, probe, "never")
    }

    /// Approve, with the commit dying at the named point.
    fn approve_crashing_at(
        fx: &Fixture,
        id: &str,
        probe: &SignerProbe,
        point: &str,
    ) -> Result<Outcome> {
        approve_with(fx, id, probe.factory(), point)
    }

    fn approve_with(
        fx: &Fixture,
        id: &str,
        make_signer: impl FnOnce() -> Result<Box<dyn Signer>>,
        point: &str,
    ) -> Result<Outcome> {
        match stage(&fx.store, Network::Signet, || Ok(fx.services()), id)? {
            Stage::Ready(staged) => {
                commit_inner(&fx.store, Network::Signet, *staged, make_signer, &|label| {
                    if label == point {
                        bail!("simulated crash at {label}")
                    }
                    Ok(())
                })
            }
            Stage::Denied(_, reason) => Ok(Outcome::Denied(reason)),
        }
    }

    fn sent(outcome: Outcome) -> (String, u64) {
        match outcome {
            Outcome::Sent { txid, fee_sat, .. } => (txid, fee_sat),
            other => panic!("expected sent, got {other:?}"),
        }
    }

    fn asset_fixture(fx: &Fixture) -> String {
        let token = fx.grant();
        let ctx = walletd::open(&fx.store, Network::Signet).unwrap();
        let asset = ctx
            .wallet
            .list_unspent()
            .find(|u| u.txout.value.to_sat() == 546)
            .unwrap()
            .outpoint;
        std::fs::write(
            fx.mockdata.join("alkanes-outpoints.json"),
            serde_json::json!({asset.to_string(): {"32:0":"5000", "2:0":"50"}}).to_string(),
        )
        .unwrap();
        token
    }

    fn asset_request(
        fx: &Fixture,
        token: &str,
        key: &str,
        id: &str,
        amount: &str,
    ) -> Result<AgentRequest, CreateError> {
        crate::request::create_alkanes_send(
            &fx.store,
            Network::Signet,
            &crate::request::CreateAlkanesSendParams {
                agent: "claude",
                token,
                idempotency_key: key,
                id,
                amount,
                address: ADDRESS,
            },
        )
    }

    #[test]
    fn asset_filing_is_local_exact_idempotent_and_uses_btc_grant_boundaries() {
        for boundary in ["observe", "amount", "recipient", "expiry", "allowed"] {
            let fx = Fixture::new(&[546, 100_000]);
            let token = asset_fixture(&fx);
            let mut grant = fx.grant_state();
            match boundary {
                "observe" => grant.mode = GrantMode::Observe,
                "amount" => grant.max_tx_sat = Some(545),
                "recipient" => grant.allowed_recipients = Some(vec![]),
                "expiry" => grant.expires_at = 0,
                _ => grant.allowed_recipients = Some(vec![ADDRESS.into()]),
            }
            fx.store.save_grant("signet", &grant).unwrap();
            // No provider configuration or wallet is needed to file.
            std::fs::remove_file(fx.dir.path().join("config.toml")).unwrap();
            std::fs::remove_file(fx.store.wallet_db_path("signet")).unwrap();
            let request =
                asset_request(&fx, &token, "asset", "032:00", &u128::MAX.to_string()).unwrap();
            assert_eq!(
                request.format_version,
                sats_core::request::ASSET_TRANSFER_REQUEST_FORMAT_VERSION
            );
            assert!(request.version_supported());
            let mut wrong_version = request.clone();
            wrong_version.format_version = sats_core::request::REQUEST_FORMAT_VERSION;
            assert!(!wrong_version.version_supported());
            assert_eq!(request.amount_sat, 546);
            assert_eq!(
                request.state.deny_reason().map(DenyReason::code),
                match boundary {
                    "observe" => Some("observe_only"),
                    "amount" => Some("over_max_tx"),
                    "recipient" => Some("recipient_not_allowed"),
                    "expiry" => Some("expired"),
                    _ => None,
                }
            );
            let repeated =
                asset_request(&fx, &token, "asset", "32:0", &u128::MAX.to_string()).unwrap();
            assert_eq!(request.id, repeated.id);
            assert!(matches!(
                asset_request(&fx, &token, "asset", "32:0", "1000"),
                Err(CreateError::Conflict { .. })
            ));
            assert!(matches!(
                fx.create_as("claude", &token, "asset", 546),
                Err(CreateError::Conflict { .. })
            ));
            assert_eq!(fx.transactions(), 0);
        }
    }

    #[test]
    fn asset_inputs_reject_bad_quantities_ids_and_unauthorized_tokens() {
        let fx = Fixture::new(&[546, 100_000]);
        let token = asset_fixture(&fx);
        for amount in [
            "",
            "0",
            "-1",
            "+1",
            "1.5",
            "340282366920938463463374607431768211456",
        ] {
            assert_eq!(
                asset_request(&fx, &token, "asset", "32:0", amount)
                    .unwrap_err()
                    .code(),
                "invalid_asset_amount"
            );
        }
        assert_eq!(
            asset_request(&fx, &token, "asset", "frBTC", "1")
                .unwrap_err()
                .code(),
            "invalid_alkane_id"
        );
        assert_eq!(
            asset_request(&fx, "wrong", "asset", "32:0", "1")
                .unwrap_err()
                .code(),
            "unauthorized"
        );
        assert!(fx.store.list_agent_requests("signet").unwrap().is_empty());
    }

    #[test]
    fn asset_approval_preserves_other_assets_and_signs_once() {
        let fx = Fixture::new(&[546, 100_000]);
        let token = asset_fixture(&fx);
        let request = asset_request(&fx, &token, "asset", "32:0", "1000").unwrap();
        let probe = SignerProbe::default();
        let Stage::Ready(staged) = stage(
            &fx.store,
            Network::Signet,
            || Ok(fx.services()),
            &request.id,
        )
        .unwrap() else {
            panic!("denied")
        };
        let review = staged.asset_transfer_review().unwrap();
        assert_eq!(review.amount, 1000);
        assert_eq!(review.summary.recipient_sat, 546);
        assert_eq!(
            review.summary.returned.get(&"32:0".parse().unwrap()),
            Some(&4000)
        );
        assert_eq!(
            review.summary.returned.get(&"2:0".parse().unwrap()),
            Some(&50)
        );
        let fee = staged.spend().fee_sat;
        assert_eq!(staged.spend().amount_sat, 546);
        assert_eq!(probe.counts(), (0, 0));
        let (_, actual_fee) =
            sent(commit(&fx.store, Network::Signet, *staged, probe.factory()).unwrap());
        assert_eq!(actual_fee, fee);
        assert_eq!(fx.transactions(), 1);
        assert_eq!(probe.counts(), (1, 1));
        assert!(approve(&fx, &request.id, &probe).is_err());
        assert_eq!(probe.counts(), (1, 1));
    }

    #[test]
    fn asset_approval_fails_closed_before_signing_on_provider_and_asset_changes() {
        for failure in [
            "unindexed",
            "missing",
            "insufficient",
            "sync",
            "changed",
            "tightened",
            "revoked",
            "replaced",
        ] {
            let fx = Fixture::new(&[546, 100_000]);
            let token = asset_fixture(&fx);
            let request = asset_request(&fx, &token, "asset", "32:0", "1000").unwrap();
            let probe = SignerProbe::default();
            match failure {
                "unindexed" => {
                    let ctx = walletd::open(&fx.store, Network::Signet).unwrap();
                    let points: serde_json::Map<String, serde_json::Value> = ctx
                        .wallet
                        .list_unspent()
                        .map(|u| (u.outpoint.to_string(), serde_json::Value::Null))
                        .collect();
                    std::fs::write(
                        fx.mockdata.join("alkanes-outpoints.json"),
                        serde_json::to_string(&points).unwrap(),
                    )
                    .unwrap();
                }
                "missing" => {
                    std::fs::remove_file(fx.mockdata.join("alkanes-outpoints.json")).unwrap();
                }
                "insufficient" => {}
                "sync" => {
                    std::fs::write(fx.mockdata.join("sync-error"), "offline").unwrap();
                }
                _ => {}
            }
            if failure == "insufficient" {
                let mut r = request.clone();
                let RequestOperation::AssetTransfer { transfer } = &mut r.operation else {
                    unreachable!()
                };
                transfer.amount = 5001;
                r.intent_digest = r.expected_digest();
                fx.store.save_agent_request("signet", &r).unwrap();
            }
            if ["changed", "tightened", "revoked", "replaced"].contains(&failure) {
                let Stage::Ready(staged) = stage(
                    &fx.store,
                    Network::Signet,
                    || Ok(fx.services()),
                    &request.id,
                )
                .unwrap() else {
                    panic!("denied")
                };
                match failure {
                    "changed" => {
                        std::fs::write(fx.mockdata.join("alkanes-outpoints.json"), "{}").unwrap();
                    }
                    "tightened" => {
                        let mut grant = fx.grant_state();
                        grant.max_tx_sat = Some(545);
                        fx.store.save_grant("signet", &grant).unwrap();
                    }
                    "revoked" => fx.revoke(),
                    _ => {
                        fx.grant();
                    }
                }
                assert!(!matches!(
                    commit(&fx.store, Network::Signet, *staged, probe.factory()).unwrap(),
                    Outcome::Sent { .. }
                ));
            } else {
                assert!(approve(&fx, &request.id, &probe).is_err());
                assert_eq!(fx.request(&request.id).status(), "failed");
            }
            assert_eq!(probe.counts(), (0, 0), "{failure}");
            assert_eq!(fx.transactions(), 0);
        }
    }

    #[test]
    fn asset_unknown_protocol_and_tampered_facts_stop_before_provider_access() {
        for field in ["protocol", "id", "quantity", "postage", "digest"] {
            let fx = Fixture::new(&[546, 100_000]);
            let token = asset_fixture(&fx);
            let mut request = asset_request(&fx, &token, "asset", "32:0", "1000").unwrap();
            let RequestOperation::AssetTransfer { transfer } = &mut request.operation else {
                unreachable!()
            };
            match field {
                "protocol" => transfer.asset.protocol = "unknown".into(),
                "id" => transfer.asset.id = "032:0".into(),
                "quantity" => transfer.amount = 1001,
                "postage" => request.amount_sat = 547,
                _ => request.recipient = "tampered".into(),
            }
            if !["quantity", "digest"].contains(&field) {
                request.intent_digest = request.expected_digest();
            }
            fx.store.save_agent_request("signet", &request).unwrap();
            assert!(
                stage(
                    &fx.store,
                    Network::Signet,
                    || panic!("must reject before I/O"),
                    &request.id
                )
                .is_err()
            );
        }
    }

    #[test]
    fn asset_signer_uncertainty_never_signs_again() {
        for fault in [Fault::Error, Fault::SignThenError, Fault::Unfinalized] {
            let fx = Fixture::new(&[546, 100_000]);
            let token = asset_fixture(&fx);
            let request = asset_request(&fx, &token, "asset", "32:0", "1000").unwrap();
            let probe = SignerProbe::with(fault);
            assert!(matches!(
                approve(&fx, &request.id, &probe).unwrap(),
                Outcome::Unresolved { .. }
            ));
            assert_eq!(fx.request(&request.id).status(), "unresolved");
            assert_eq!(probe.counts(), (1, 1));
            assert!(approve(&fx, &request.id, &probe).is_err());
            assert_eq!(probe.counts(), (1, 1));
        }
    }

    #[test]
    fn asset_review_cannot_be_replaced_and_a_tightened_grant_denies_before_signing() {
        for change in ["asset", "quantity", "recipient", "tightened"] {
            let fx = Fixture::new(&[546, 100_000]);
            let token = asset_fixture(&fx);
            let mut request = asset_request(&fx, &token, "asset", "32:0", "1000").unwrap();
            let Stage::Ready(staged) = stage(
                &fx.store,
                Network::Signet,
                || Ok(fx.services()),
                &request.id,
            )
            .unwrap() else {
                panic!("denied")
            };
            if change == "tightened" {
                drop(staged);
                let mut grant = fx.grant_state();
                grant.max_tx_sat = Some(545);
                fx.store.save_grant("signet", &grant).unwrap();
                let Stage::Denied(_, reason) = stage(
                    &fx.store,
                    Network::Signet,
                    || Ok(fx.services()),
                    &request.id,
                )
                .unwrap() else {
                    panic!("not denied")
                };
                assert!(matches!(reason, DenyReason::OverMaxTx { .. }));
            } else {
                let RequestOperation::AssetTransfer { transfer } = &mut request.operation else {
                    unreachable!()
                };
                match change {
                    "asset" => transfer.asset.id = "2:0".into(),
                    "quantity" => transfer.amount += 1,
                    _ => {
                        request.recipient = staged
                            .ctx
                            .wallet
                            .peek_address(sats_core::bdk_wallet::KeychainKind::External, 1)
                            .address
                            .to_string()
                    }
                }
                request.intent_digest = request.expected_digest();
                fx.store.save_agent_request("signet", &request).unwrap();
                let probe = SignerProbe::default();
                assert!(commit(&fx.store, Network::Signet, *staged, probe.factory()).is_err());
                assert_eq!(probe.counts(), (0, 0));
            }
        }
    }

    #[test]
    fn asset_crash_and_broadcast_failure_preserve_the_existing_recovery_boundary() {
        for failure in ["signing-saved", "broadcast"] {
            let fx = Fixture::new(&[546, 100_000]);
            let token = asset_fixture(&fx);
            let request = asset_request(&fx, &token, "asset", "32:0", "1000").unwrap();
            let probe = SignerProbe::default();
            if failure == "signing-saved" {
                assert!(approve_crashing_at(&fx, &request.id, &probe, failure).is_err());
                assert!(approve(&fx, &request.id, &probe).is_err());
                assert_eq!(fx.request(&request.id).status(), "unresolved");
                assert_eq!(probe.counts(), (0, 0));
            } else {
                std::fs::write(fx.mockdata.join("broadcast-fail"), "offline").unwrap();
                let Outcome::BroadcastPending { txid, .. } =
                    approve(&fx, &request.id, &probe).unwrap()
                else {
                    panic!("not pending")
                };
                assert!(approve(&fx, &request.id, &probe).is_err());
                std::fs::remove_file(fx.mockdata.join("broadcast-fail")).unwrap();
                crate::spend::rebroadcast(&fx.store, Network::Signet, || Ok(fx.services()), &txid)
                    .unwrap();
                assert_eq!(probe.counts(), (1, 1));
                assert_eq!(fx.transactions(), 1);
            }
        }
    }

    fn wrap_fixture(fx: &Fixture) -> String {
        let token = fx.grant();
        std::fs::write(
            fx.mockdata.join("alkanes-bytecode.json"),
            r#"{"32:0":"0x0061736d"}"#,
        )
        .unwrap();
        std::fs::write(
            fx.mockdata.join("alkanes-signer.hex"),
            "0a241a22512030685674457c3eaa6d2904a1340ab80296625e8c7313f54c6ae7c51318f5860710e1fc02",
        )
        .unwrap();
        std::fs::write(fx.mockdata.join("alkanes-wrap.hex"), "0a5e0a0d0a060a0208201200120308864e122b0a262f7365656e2fffdbcfe2faaab58dcfe6363d4cad67bf68ccae88d3e55e0a685a440e35a85f7412010112200a0c2f746f74616c737570706c7912107acbb22f0100000000000000000000001093880f").unwrap();
        token
    }

    fn wrap_request(fx: &Fixture, token: &str) -> AgentRequest {
        crate::request::create_wrap(
            &fx.store,
            Network::Signet,
            &crate::request::CreateWrapParams {
                agent: "claude",
                token,
                idempotency_key: "wrap-1",
                amount_sat: 10_000,
            },
            || Ok(fx.services()),
        )
        .unwrap()
    }

    #[test]
    fn wrap_filing_uses_btc_limits_and_rechecks_authority_after_discovery() {
        for boundary in ["observe", "amount", "recipient", "allowed"] {
            let fx = Fixture::new(&[100_000]);
            let token = wrap_fixture(&fx);
            let mut grant = fx.grant_state();
            match boundary {
                "observe" => grant.mode = GrantMode::Observe,
                "amount" => grant.max_tx_sat = Some(9_999),
                "recipient" => grant.allowed_recipients = Some(vec![ADDRESS.into()]),
                _ => {
                    grant.allowed_recipients =
                        Some(vec![wrapping::info(&fx.services()).unwrap().signer_address])
                }
            }
            fx.store.save_grant("signet", &grant).unwrap();
            let request = wrap_request(&fx, &token);
            let reason = request.state.deny_reason().map(DenyReason::code);
            assert_eq!(
                reason,
                match boundary {
                    "observe" => Some("observe_only"),
                    "amount" => Some("over_max_tx"),
                    "recipient" => Some("recipient_not_allowed"),
                    _ => None,
                },
                "{boundary}"
            );
            assert_eq!(fx.transactions(), 0);
        }
        for action in ["revoke", "replace", "expire"] {
            let fx = Fixture::new(&[100_000]);
            let token = wrap_fixture(&fx);
            let result = crate::request::create_wrap(
                &fx.store,
                Network::Signet,
                &crate::request::CreateWrapParams {
                    agent: "claude",
                    token: &token,
                    idempotency_key: "race",
                    amount_sat: 10_000,
                },
                || {
                    match action {
                        "revoke" => fx.revoke(),
                        "replace" => {
                            fx.grant();
                        }
                        _ => {
                            let mut grant = fx.grant_state();
                            grant.expires_at = 0;
                            fx.store.save_grant("signet", &grant).unwrap();
                        }
                    }
                    Ok(fx.services())
                },
            );
            match action {
                "revoke" => assert!(matches!(result, Err(CreateError::NoGrant))),
                "replace" => assert!(matches!(result, Err(CreateError::Unauthorized))),
                _ => assert_eq!(
                    result.unwrap().state.deny_reason().unwrap().code(),
                    "expired"
                ),
            }
            assert_eq!(fx.transactions(), 0);
        }
    }

    #[test]
    fn wrap_request_recipient_cannot_disagree_with_its_actual_signer() {
        let fx = Fixture::new(&[100_000]);
        let token = wrap_fixture(&fx);
        let mut request = wrap_request(&fx, &token);
        request.recipient = ADDRESS.into();
        request.intent_digest = request.expected_digest();
        fx.store.save_agent_request("signet", &request).unwrap();
        let result = stage(
            &fx.store,
            Network::Signet,
            || panic!("must reject before provider I/O"),
            &request.id,
        );
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("signer does not match")
        );
    }

    #[test]
    fn wrap_approval_verifies_outputs_and_signs_once_for_gross_btc() {
        let fx = Fixture::new(&[100_000]);
        let token = wrap_fixture(&fx);
        let request = wrap_request(&fx, &token);
        assert_eq!(request.status(), "pending_approval");
        assert_eq!(
            request.format_version,
            sats_core::request::WRAP_REQUEST_FORMAT_VERSION
        );
        assert_ne!(
            request.intent_digest,
            sats_core::intent::SendIntent {
                network: request.network.clone(),
                agent: request.agent.clone(),
                recipient: request.recipient.clone(),
                amount_sat: request.amount_sat,
            }
            .digest()
        );
        let Stage::Ready(staged) = stage(
            &fx.store,
            Network::Signet,
            || Ok(fx.services()),
            &request.id,
        )
        .unwrap() else {
            panic!("denied")
        };
        let review = staged.wrap_review().unwrap();
        assert_eq!(review.predicted_frbtc, 9990);
        assert_eq!(review.postage_sat, 546);
        assert_eq!(
            staged.prepared.psbt().unsigned_tx.output[0].value.to_sat(),
            10_000
        );
        // The amount the grant's cap and the human see is the gross BTC.
        let spend = *staged.spend();
        assert_eq!(spend.amount_sat, 10_000);
        assert_eq!(fx.transactions(), 0);
        let probe = SignerProbe::default();
        let (_, fee) = sent(commit(&fx.store, Network::Signet, *staged, probe.factory()).unwrap());
        assert_eq!(fee, spend.fee_sat);
        assert_eq!(probe.counts(), (1, 1));
        assert_eq!(fx.request(&request.id).operation, request.operation);
        assert!(approve(&fx, &request.id, &probe).is_err());
        assert_eq!(probe.counts(), (1, 1));
    }

    #[test]
    fn wrap_failures_before_signing_never_sign_and_changes_after_review_are_refused() {
        for failure in [
            "hash",
            "signer",
            "simulation",
            "unknown-utxo",
            "sync",
            "asset-utxo",
        ] {
            let fx = Fixture::new(&[100_000]);
            let token = wrap_fixture(&fx);
            let request = wrap_request(&fx, &token);
            match failure {
                "hash" => std::fs::write(
                    fx.mockdata.join("alkanes-bytecode.json"),
                    r#"{"32:0":"0x00"}"#,
                )
                .unwrap(),
                "signer" => std::fs::write(fx.mockdata.join("alkanes-signer.hex"), "0a00").unwrap(),
                "simulation" => {
                    std::fs::write(fx.mockdata.join("alkanes-wrap.hex"), "0a001a03626164").unwrap()
                }
                "sync" => std::fs::write(fx.mockdata.join("sync-error"), "offline").unwrap(),
                _ => {
                    let ctx = walletd::open(&fx.store, Network::Signet).unwrap();
                    let outpoint = ctx
                        .wallet
                        .list_unspent()
                        .next()
                        .unwrap()
                        .outpoint
                        .to_string();
                    let value = if failure == "unknown-utxo" {
                        serde_json::Value::Null
                    } else {
                        serde_json::json!({"32:0":"1"})
                    };
                    std::fs::write(
                        fx.mockdata.join("alkanes-outpoints.json"),
                        serde_json::json!({outpoint:value}).to_string(),
                    )
                    .unwrap();
                }
            }
            let probe = SignerProbe::default();
            assert!(approve(&fx, &request.id, &probe).is_err(), "{failure}");
            assert_eq!(probe.counts(), (0, 0), "{failure}");
            assert_eq!(fx.request(&request.id).status(), "failed");
        }
        for changed in ["allowlist", "request", "provider", "contract"] {
            let fx = Fixture::new(&[100_000]);
            let token = wrap_fixture(&fx);
            let request = wrap_request(&fx, &token);
            let Stage::Ready(staged) = stage(
                &fx.store,
                Network::Signet,
                || Ok(fx.services()),
                &request.id,
            )
            .unwrap() else {
                panic!("denied")
            };
            match changed {
                "allowlist" => {
                    let mut grant = fx.grant_state();
                    grant.allowed_recipients = Some(vec![ADDRESS.into()]);
                    fx.store.save_grant("signet", &grant).unwrap();
                }
                "request" => {
                    let mut changed = request.clone();
                    changed.amount_sat += 1;
                    changed.intent_digest = changed.expected_digest();
                    fx.store.save_agent_request("signet", &changed).unwrap();
                }
                "contract" => std::fs::write(
                    fx.mockdata.join("alkanes-bytecode.json"),
                    r#"{"32:0":"0x00"}"#,
                )
                .unwrap(),
                _ => std::fs::remove_file(fx.mockdata.join("alkanes-signer.hex")).unwrap(),
            }
            let probe = SignerProbe::default();
            let result = commit(&fx.store, Network::Signet, *staged, probe.factory());
            match changed {
                "allowlist" => assert!(matches!(
                    result.unwrap(),
                    Outcome::Denied(DenyReason::RecipientNotAllowed { .. })
                )),
                "provider" | "contract" => {
                    assert!(matches!(result.unwrap(), Outcome::Failed { .. }))
                }
                _ => assert!(result.is_err()),
            }
            assert_eq!(probe.counts(), (0, 0));
        }
    }

    #[test]
    fn wrap_signer_errors_and_crashes_never_sign_again() {
        for point in ["signer", "signing-saved"] {
            let fx = Fixture::new(&[100_000]);
            let token = wrap_fixture(&fx);
            let request = wrap_request(&fx, &token);
            let probe = SignerProbe::with(Fault::SignThenError);
            if point == "signer" {
                assert!(matches!(
                    approve(&fx, &request.id, &probe).unwrap(),
                    Outcome::Unresolved { .. }
                ));
                assert_eq!(probe.counts(), (1, 1));
            } else {
                assert!(approve_crashing_at(&fx, &request.id, &probe, point).is_err());
                assert_eq!(probe.counts(), (0, 0));
            }
            assert!(approve(&fx, &request.id, &probe).is_err());
            assert_eq!(fx.request(&request.id).status(), "unresolved");
        }
    }

    // ---- creation and identity -----------------------------------------

    #[test]
    fn create_records_a_pending_request_bound_to_its_grant() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let first = fx.create(&token, "job-1", 10_000);
        let grant = fx.grant_state();
        assert!(is_request_id(&first.id), "{}", first.id);
        assert_eq!(
            first.id,
            keyed_request_id(&grant.grant_id, "claude", "job-1")
        );
        assert_eq!(first.idempotency_key.as_deref(), Some("job-1"));
        assert_eq!(first.state, RequestState::PendingApproval);
        assert_eq!(first.grant_id, grant.grant_id);
        assert_eq!(grant.grant_id.len(), 32);
        assert_eq!(fx.events(), vec!["request_received"]);
        // Same key, same intent: the same record, no new event.
        let again = fx.create(&token, "job-1", 10_000);
        assert_eq!(again.id, first.id);
        assert_eq!(fx.events(), vec!["request_received"]);
        // Same key, different intent: a conflict, nothing written.
        let conflict = fx.create_as("claude", &token, "job-1", 20_000).unwrap_err();
        let CreateError::Conflict { request_id } = conflict else {
            panic!("expected conflict, got {conflict:?}");
        };
        assert_eq!(request_id, first.id);
        assert_eq!(fx.request(&first.id).amount_sat, 10_000);
    }

    /// The same client key from two agents names two requests; the id a
    /// filing returns selects exactly one of them, and the shared prefix
    /// every id carries is refused as ambiguous rather than resolved to
    /// whichever record came first.
    #[test]
    fn same_client_key_across_agents_yields_distinct_global_ids() {
        let fx = Fixture::new(&[100_000, 100_000]);
        let alice = fx.grant_for("alice");
        let bob = fx.grant_for("bob");
        let a = fx.create_as("alice", &alice, "pay", 10_000).unwrap();
        let b = fx.create_as("bob", &bob, "pay", 10_000).unwrap();
        assert_ne!(a.id, b.id);
        assert!(is_request_id(&a.id) && is_request_id(&b.id));
        assert_eq!(
            fx.store.find_agent_request("signet", &a.id).unwrap().agent,
            "alice"
        );
        assert_eq!(
            fx.store.find_agent_request("signet", &b.id).unwrap().agent,
            "bob"
        );
        let err = fx.store.find_agent_request("signet", "r-").unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err:#}");

        // Dismissing by bob's id touches bob's record only.
        dismiss(&fx.store, Network::Signet, &b.id).unwrap();
        assert_eq!(fx.request_of("bob", &b.id).status(), "dismissed");
        assert_eq!(fx.request_of("alice", &a.id).status(), "pending_approval");
        // Approving by alice's id executes alice's record only.
        let probe = SignerProbe::default();
        let (txid, _) = sent(approve(&fx, &a.id, &probe).unwrap());
        assert_eq!(
            fx.request_of("alice", &a.id).state.txid(),
            Some(txid.as_str())
        );
        assert_eq!(fx.request_of("bob", &b.id).status(), "dismissed");
        assert_eq!(probe.counts(), (1, 1));
    }

    /// A key reused after a revoke and re-issue never resolves to the
    /// old grant's request: the id commits to the grant, so the second
    /// filing is a new request under the new grant, and the old one
    /// stays bound to the grant that is gone.
    #[test]
    fn same_client_key_after_reissue_files_a_new_request() {
        let fx = Fixture::new(&[100_000]);
        let old = fx.grant();
        let first = fx.create(&old, "invoice", 10_000);
        fx.revoke();
        let fresh = fx.grant();
        let second = fx.create(&fresh, "invoice", 10_000);
        assert_ne!(second.id, first.id);
        assert_eq!(second.state, RequestState::PendingApproval);
        assert_eq!(second.grant_id, fx.grant_state().grant_id);
        assert_ne!(first.grant_id, second.grant_id);
        assert_eq!(fx.events(), vec!["request_received", "request_received"]);
        // Repeating the key under the new grant still deduplicates there.
        assert_eq!(fx.create(&fresh, "invoice", 10_000).id, second.id);
        // The old record is still on file, still bound to the old grant.
        let probe = SignerProbe::default();
        assert_eq!(
            approve(&fx, &first.id, &probe).unwrap(),
            Outcome::Denied(DenyReason::Revoked)
        );
        assert_eq!(probe.counts(), (0, 0));
        assert!(matches!(
            approve(&fx, &second.id, &probe).unwrap(),
            Outcome::Sent { .. }
        ));
        assert_eq!(probe.counts(), (1, 1));
    }

    #[test]
    fn create_refuses_a_bad_token_and_writes_nothing() {
        let fx = Fixture::new(&[100_000]);
        let _token = fx.grant();
        let err = fx
            .create_as("claude", "not-the-token", "job-1", 10_000)
            .unwrap_err();
        assert_eq!(err.code(), "unauthorized");
        assert!(fx.store.list_agent_requests("signet").unwrap().is_empty());
        assert!(fx.events().is_empty());
    }

    /// The audit line comes first: an unwritable log refuses the create
    /// and no request record exists without its causal event.
    #[test]
    fn create_refuses_when_the_audit_log_cannot_be_written() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        // A file where the events directory must be: every append fails.
        std::fs::write(fx.dir.path().join("signet/events"), b"in the way").unwrap();
        let err = fx.create_as("claude", &token, "job-1", 10_000).unwrap_err();
        assert_eq!(err.code(), "store_error");
        assert!(err.message("claude").contains("cannot record request"));
        assert!(
            fx.store.list_agent_requests("signet").unwrap().is_empty(),
            "no record without its audit line"
        );
    }

    #[test]
    fn create_records_a_hard_boundary_as_denied() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let mut grant = fx.grant_state();
        grant.max_tx_sat = Some(5_000);
        fx.store.save_grant("signet", &grant).unwrap();
        let denied = fx.create(&token, "big", 10_000);
        assert!(matches!(
            denied.state,
            RequestState::Denied {
                deny: DenyReason::OverMaxTx { .. },
                ..
            }
        ));
        assert!(!denied.is_approvable());
        assert_eq!(fx.events(), vec!["request_received", "denied"]);
        // Approving a denied request is refused before anything runs.
        let probe = SignerProbe::default();
        let err = approve(&fx, &denied.id, &probe).unwrap_err();
        assert!(err.to_string().contains("denied over_max_tx"), "{err:#}");
        assert_eq!(probe.counts(), (0, 0));
    }

    // ---- grant binding -------------------------------------------------

    /// A pending request outlives its grant: once the grant is revoked
    /// the request is non-executable and records why.
    #[test]
    fn revoked_grant_denies_the_pending_request() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        fx.revoke();
        let probe = SignerProbe::default();
        let outcome = approve(&fx, &id, &probe).unwrap();
        assert_eq!(outcome, Outcome::Denied(DenyReason::Revoked));
        assert_eq!(probe.counts(), (0, 0));
        let request = fx.request(&id);
        assert_eq!(request.state.deny_reason(), Some(&DenyReason::Revoked));
        assert!(!request.is_approvable());
        assert_eq!(fx.events(), vec!["request_received", "denied"]);
    }

    /// A pending request outlives its grant's expiry too: the grant is
    /// still on disk, so the request stays bound to it, and the ladder
    /// denies it as expired without constructing a signer.
    #[test]
    fn expired_grant_denies_the_pending_request() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        let mut grant = fx.grant_state();
        grant.expires_at = 1;
        {
            let _lock = fx.store.lock_grants("signet").unwrap();
            fx.store.save_grant("signet", &grant).unwrap();
        }
        let probe = SignerProbe::default();
        let outcome = approve(&fx, &id, &probe).unwrap();
        let expired = DenyReason::Expired { expired_at: 1 };
        assert_eq!(outcome, Outcome::Denied(expired.clone()));
        assert_eq!(probe.counts(), (0, 0));
        let request = fx.request(&id);
        assert_eq!(request.state.deny_reason(), Some(&expired));
        assert!(!request.is_approvable());
        assert_eq!(fx.events(), vec!["request_received", "denied"]);
    }

    /// A re-issued grant for the same agent never inherits the old
    /// grant's requests: revoke, re-grant, approve still refuses.
    #[test]
    fn reissued_grant_does_not_execute_the_old_grants_request() {
        let fx = Fixture::new(&[100_000]);
        let old = fx.grant();
        let request = fx.create(&old, "pay", 10_000);
        fx.revoke();
        let fresh = fx.grant();
        assert_ne!(request.grant_id, fx.grant_state().grant_id);
        let probe = SignerProbe::default();
        assert_eq!(
            approve(&fx, &request.id, &probe).unwrap(),
            Outcome::Denied(DenyReason::Revoked)
        );
        assert_eq!(probe.counts(), (0, 0));
        assert_eq!(fx.request(&request.id).status(), "denied");
        // The old token cannot file under the new grant either.
        assert_eq!(
            fx.create_as("claude", &old, "pay-2", 10_000)
                .unwrap_err()
                .code(),
            "unauthorized"
        );
        // A request filed under the new grant executes normally.
        let id = fx.create(&fresh, "pay-3", 10_000).id;
        assert!(matches!(
            approve(&fx, &id, &probe).unwrap(),
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
        let old = fx.grant();
        let old_id = fx.grant_state().grant_id.clone();

        // Hold the grant lock, start the create, then revoke and re-grant
        // while the create is blocked on the lock.
        let lock = fx.store.lock_grants("signet").unwrap();
        let dir = fx.dir.path().to_path_buf();
        let old_token = old.clone();
        let worker = std::thread::spawn(move || {
            let store = Store::open(&dir);
            create(
                &store,
                Network::Signet,
                &CreateParams {
                    agent: "claude",
                    token: &old_token,
                    idempotency_key: "racy",
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
        fx.store
            .save_grant("signet", &fresh_grant("claude", &minted))
            .unwrap();
        drop(lock);

        // The create authenticated after the swap: the old token is dead.
        assert_eq!(worker.join().unwrap().unwrap_err(), "unauthorized");
        assert!(
            fx.store.list_agent_requests("signet").unwrap().is_empty(),
            "nothing was filed under either grant"
        );
        assert!(fx.events().is_empty());
        assert_ne!(fx.grant_state().grant_id, old_id);
    }

    // ---- execution -----------------------------------------------------

    /// The whole loop: create → approve → sent, with exactly one signer
    /// construction and one signature.
    #[test]
    fn approve_executes_once_and_settles_to_sent() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        let probe = SignerProbe::default();
        let outcome = approve(&fx, &id, &probe).unwrap();
        let Outcome::Sent {
            txid,
            amount_sat,
            fee_sat,
        } = outcome
        else {
            panic!("expected sent, got {outcome:?}");
        };
        assert_eq!(probe.counts(), (1, 1));
        assert_eq!(amount_sat, 10_000);
        assert!(fee_sat > 0);

        let request = fx.request(&id);
        assert_eq!(request.state.txid(), Some(txid.as_str()));
        assert_eq!(request.status(), "sent");
        let record = fx.store.load_transaction("signet", &txid).unwrap();
        assert_eq!(record.status, TransactionStatus::Broadcast);
        let origin = record.origin.as_ref().unwrap();
        assert_eq!(origin.agent.as_deref(), Some("claude"));
        assert_eq!(origin.request_id.as_deref(), Some(id.as_str()));
        assert_eq!(
            origin.intent_digest.as_deref(),
            Some(request.intent_digest.as_str())
        );
        assert_eq!(
            fx.events(),
            vec!["request_received", "approved", "signed", "broadcast"]
        );
        assert!(
            std::fs::read_to_string(fx.mockdata.join("broadcasts.log"))
                .unwrap()
                .contains(&txid)
        );

        // A settled request cannot be approved or dismissed again: the
        // signer is never reached twice for one request, and listing
        // never changes it.
        let again = approve(&fx, &id, &probe).unwrap_err();
        assert!(again.to_string().contains("already sent"), "{again:#}");
        assert_eq!(probe.counts(), (1, 1));
        assert!(dismiss(&fx.store, Network::Signet, &id).is_err());
        list_reconciled(&fx.store, Network::Signet).unwrap();
        assert_eq!(fx.request(&id).status(), "sent");
        assert_eq!(fx.transactions(), 1);
    }

    // ---- the signer boundary -------------------------------------------

    /// The signer returns an error. Its word is not trusted: once
    /// invoked, the request is unresolved and it is never signed again.
    #[test]
    fn signer_error_after_invocation_is_unresolved() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        let failing = SignerProbe::with(Fault::Error);
        let outcome = approve(&fx, &id, &failing).unwrap();
        let Outcome::Unresolved { txid, message } = outcome else {
            panic!("expected unresolved, got {outcome:?}");
        };
        assert!(txid.is_some());
        assert!(message.contains("a signature may exist"), "{message}");
        assert_eq!(failing.counts(), (1, 1));
        let request = fx.request(&id);
        assert_eq!(request.status(), "unresolved");
        assert!(!request.is_approvable());
        assert_eq!(fx.transactions(), 0);
        assert_eq!(
            fx.events(),
            vec!["request_received", "approved", "unresolved"]
        );
        // No second invocation, however the human asks, and no
        // reconciliation ever re-enables it.
        let probe = SignerProbe::default();
        let again = approve(&fx, &id, &probe).unwrap_err();
        assert!(again.to_string().contains("unresolved"), "{again:#}");
        assert_eq!(probe.counts(), (0, 0));
        list_reconciled(&fx.store, Network::Signet).unwrap();
        assert_eq!(fx.request(&id).status(), "unresolved");
    }

    /// A signer that signs and then lies about it. The error must not
    /// buy a second invocation: the signed transaction it produced is
    /// spendable.
    #[test]
    fn malicious_signer_that_signs_then_errors_gets_no_retry() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        let malicious = SignerProbe::with(Fault::SignThenError);
        let outcome = approve(&fx, &id, &malicious).unwrap();
        assert!(matches!(outcome, Outcome::Unresolved { .. }), "{outcome:?}");
        assert_eq!(malicious.counts(), (1, 1));
        let request = fx.request(&id);
        assert_eq!(request.status(), "unresolved");
        assert!(request.state.may_have_signature());
        assert_eq!(fx.transactions(), 0, "nothing durable, and still no retry");

        // Every later path: approve, list, dismiss — zero signer builds,
        // zero signatures.
        let probe = SignerProbe::default();
        assert!(approve(&fx, &id, &probe).is_err());
        list_reconciled(&fx.store, Network::Signet).unwrap();
        assert!(approve(&fx, &id, &probe).is_err());
        assert_eq!(probe.counts(), (0, 0));
        dismiss(&fx.store, Network::Signet, &id).unwrap();
        assert_eq!(fx.request(&id).status(), "dismissed");
        assert!(approve(&fx, &id, &probe).is_err());
        assert_eq!(probe.counts(), (0, 0));
    }

    /// A signer that cannot be constructed never ran: the request is
    /// `failed`, re-approvable, and the retry signs exactly once.
    #[test]
    fn signer_construction_failure_is_failed_and_the_retry_signs_once() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        let outcome = approve_with(&fx, &id, || bail!("no signer available"), "never").unwrap();
        assert!(matches!(outcome, Outcome::Failed { .. }), "{outcome:?}");
        let request = fx.request(&id);
        assert_eq!(request.status(), "failed");
        assert!(request.is_approvable());
        assert_eq!(fx.transactions(), 0);
        assert_eq!(fx.events(), vec!["request_received", "approved", "failed"]);

        let probe = SignerProbe::default();
        sent(approve(&fx, &id, &probe).unwrap());
        assert_eq!(probe.counts(), (1, 1));
        assert_eq!(fx.transactions(), 1);
    }

    /// The audit log cannot take the approval: the execution stops
    /// before the signer exists, and the request is `failed` and
    /// re-approvable.
    #[test]
    fn unwritable_audit_log_at_approval_fails_before_the_signer() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        let Stage::Ready(staged) =
            stage(&fx.store, Network::Signet, || Ok(fx.services()), &id).unwrap()
        else {
            panic!("expected ready");
        };
        // A directory where the log file must be: every append fails.
        let log = fx.dir.path().join("signet/events/log.jsonl");
        std::fs::remove_file(&log).unwrap();
        std::fs::create_dir(&log).unwrap();
        let probe = SignerProbe::default();
        let outcome = commit(&fx.store, Network::Signet, *staged, probe.factory()).unwrap();
        let Outcome::Failed { message } = outcome else {
            panic!("expected failed, got {outcome:?}");
        };
        assert!(message.contains("cannot record the approval"), "{message}");
        assert_eq!(probe.counts(), (0, 0));
        let request = fx.request(&id);
        assert_eq!(request.status(), "failed");
        assert!(request.is_approvable());

        std::fs::remove_dir(&log).unwrap();
        sent(approve(&fx, &id, &probe).unwrap());
        assert_eq!(probe.counts(), (1, 1));
    }

    /// The signer signed but the transaction could not be finalized:
    /// a signature exists in memory, so the request is never signed
    /// again.
    #[test]
    fn finalization_failure_after_signing_is_unresolved() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        let probe = SignerProbe::with(Fault::Corrupt);
        let outcome = approve(&fx, &id, &probe).unwrap();
        let Outcome::Unresolved { txid, .. } = outcome else {
            panic!("expected unresolved, got {outcome:?}");
        };
        assert!(txid.is_some(), "the unsigned txid is known");
        assert_eq!(probe.counts(), (1, 1));
        let request = fx.request(&id);
        assert_eq!(request.status(), "unresolved");
        assert!(request.state.may_have_signature());
        assert!(!request.is_approvable());
        assert_eq!(fx.transactions(), 0);
        assert_eq!(
            fx.events(),
            vec!["request_received", "approved", "unresolved"]
        );
        // Never a second signature, however the human asks.
        let again = approve(&fx, &id, &probe).unwrap_err();
        assert!(again.to_string().contains("unresolved"), "{again:#}");
        assert_eq!(probe.counts(), (1, 1));
        list_reconciled(&fx.store, Network::Signet).unwrap();
        assert_eq!(fx.request(&id).status(), "unresolved");
        // The human closes it; it is still never signed again.
        dismiss(&fx.store, Network::Signet, &id).unwrap();
        assert_eq!(fx.request(&id).status(), "dismissed");
        assert!(approve(&fx, &id, &probe).is_err());
        assert_eq!(probe.counts(), (1, 1));
    }

    /// The signer signed but left the PSBT unfinalized: a partial
    /// signature may exist. Same rule.
    #[test]
    fn unfinalized_signature_is_unresolved() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        let probe = SignerProbe::with(Fault::Unfinalized);
        let outcome = approve(&fx, &id, &probe).unwrap();
        assert!(matches!(outcome, Outcome::Unresolved { .. }), "{outcome:?}");
        assert_eq!(fx.request(&id).status(), "unresolved");
        assert!(approve(&fx, &id, &probe).is_err());
        assert_eq!(probe.counts(), (1, 1));
    }

    /// The signature could not be persisted: it exists in memory, so no
    /// second signature.
    #[test]
    fn transaction_persistence_failure_after_signing_is_unresolved() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        // A file where the transactions directory must be.
        std::fs::write(fx.dir.path().join("signet/transactions"), b"in the way").unwrap();
        let probe = SignerProbe::default();
        let outcome = approve(&fx, &id, &probe).unwrap();
        let Outcome::Unresolved { txid, message } = outcome else {
            panic!("expected unresolved, got {outcome:?}");
        };
        assert!(message.contains("cannot save"), "{message}");
        assert!(txid.is_some());
        assert_eq!(probe.counts(), (1, 1));
        assert_eq!(fx.request(&id).status(), "unresolved");
        assert!(approve(&fx, &id, &probe).is_err());
        assert_eq!(probe.counts(), (1, 1), "never signs again");
    }

    /// Broadcast failure after signing: the signature is durable, the
    /// request is `broadcast_pending`, and a second approve never signs
    /// again.
    #[test]
    fn broadcast_failure_is_broadcast_pending_and_never_signs_again() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        std::fs::write(fx.mockdata.join("broadcast-fail"), "provider down").unwrap();
        let probe = SignerProbe::default();
        let outcome = approve(&fx, &id, &probe).unwrap();
        let Outcome::BroadcastPending { txid, .. } = outcome else {
            panic!("expected broadcast_pending, got {outcome:?}");
        };
        assert_eq!(probe.counts(), (1, 1));
        let request = fx.request(&id);
        assert_eq!(request.status(), "broadcast_pending");
        assert!(request.state.has_signature());
        assert!(!request.is_approvable());
        let record = fx.store.load_transaction("signet", &txid).unwrap();
        assert_eq!(record.status, TransactionStatus::Pending);
        assert_eq!(
            fx.events(),
            vec!["request_received", "approved", "signed", "broadcast_failed"]
        );
        let again = approve(&fx, &id, &probe).unwrap_err();
        assert!(
            again.to_string().contains("signed; broadcast unconfirmed"),
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
        assert_eq!(fx.request(&id).status(), "sent");
        assert_eq!(fx.transactions(), 1, "one transaction, one signature");
        assert_eq!(probe.counts(), (1, 1));
    }

    // ---- crash windows around the signer -------------------------------

    /// `signing` is durable, then the process dies. Even though this
    /// crash is before the signer was constructed, nothing durable can
    /// tell — so the request is unresolved and nothing is signed.
    #[test]
    fn crash_after_signing_is_persisted_is_unresolved_never_resigned() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        let probe = SignerProbe::default();
        approve_crashing_at(&fx, &id, &probe, "signing-saved").unwrap_err();
        assert_eq!(probe.counts(), (0, 0));
        assert_eq!(fx.request(&id).status(), "signing");

        let settled = list_reconciled(&fx.store, Network::Signet).unwrap();
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].status(), "unresolved");
        assert_eq!(fx.events(), vec!["request_received", "unresolved"]);
        list_reconciled(&fx.store, Network::Signet).unwrap();
        assert!(approve(&fx, &id, &probe).is_err());
        assert_eq!(probe.counts(), (0, 0));
    }

    /// The same window, fabricated the way an old record would look:
    /// `signing` on disk, no transaction.
    #[test]
    fn crash_around_the_signer_is_unresolved_never_resigned() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let mut request = fx.create(&token, "pay", 10_000);
        request.state = RequestState::Signing {
            approved_at: unix_now(),
            fee_sat: 300,
        };
        fx.store.save_agent_request("signet", &request).unwrap();

        let settled = list_reconciled(&fx.store, Network::Signet).unwrap();
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].status(), "unresolved");
        assert!(!settled[0].is_approvable());
        assert_eq!(fx.events(), vec!["request_received", "unresolved"]);
        // Reconciling again is a no-op, and approving refuses.
        list_reconciled(&fx.store, Network::Signet).unwrap();
        assert_eq!(fx.events().len(), 2);
        let probe = SignerProbe::default();
        assert!(approve(&fx, &request.id, &probe).is_err());
        assert_eq!(probe.counts(), (0, 0));
    }

    /// Crash after the signature was persisted: the request becomes
    /// `broadcast_pending` (or `sent` if the record says so), and it is
    /// never signed again.
    #[test]
    fn crash_after_persisted_signature_reconciles_without_resigning() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        // Run a real execution, then rewind the request to `signing`
        // as a crash between persisting the signature and settling would
        // leave it.
        std::fs::write(fx.mockdata.join("broadcast-fail"), "down").unwrap();
        let probe = SignerProbe::default();
        let Outcome::BroadcastPending { txid, fee_sat, .. } = approve(&fx, &id, &probe).unwrap()
        else {
            panic!("expected broadcast_pending");
        };
        let mut request = fx.request(&id);
        request.state = RequestState::Signing {
            approved_at: unix_now(),
            fee_sat,
        };
        fx.store.save_agent_request("signet", &request).unwrap();

        let settled = list_reconciled(&fx.store, Network::Signet).unwrap();
        assert_eq!(settled[0].status(), "broadcast_pending");
        assert_eq!(settled[0].state.txid(), Some(txid.as_str()));
        let again = approve(&fx, &id, &probe).unwrap_err();
        assert!(again.to_string().contains("signed; broadcast unconfirmed"));
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
    }

    /// A transaction settles only the request it is fully attributed to:
    /// agent, request id, and intent digest must all match.
    #[test]
    fn reconciliation_matches_the_full_attribution() {
        let fx = Fixture::new(&[100_000, 100_000]);
        let alice = fx.grant_for("alice");
        let bob = fx.grant_for("bob");
        let alice_request = fx.create_as("alice", &alice, "pay", 10_000).unwrap();
        let bob_request = fx.create_as("bob", &bob, "pay", 10_000).unwrap();
        assert_ne!(alice_request.intent_digest, bob_request.intent_digest);
        let probe = SignerProbe::default();
        let services = fx.services();
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
        let mut record = prepared.into_transaction(psbt).unwrap();
        // First attributed to bob's request id with alice's digest: two
        // of three attributes is not attribution.
        record = record.with_origin(TxOrigin {
            surface: "agent".into(),
            agent: Some("bob".into()),
            request_id: Some(bob_request.id.clone()),
            intent_digest: Some(alice_request.intent_digest.clone()),
        });
        fx.store.save_transaction("signet", &record).unwrap();

        // Bob's request was interrupted while signing. The record does
        // not settle him.
        let mut bob_signing = bob_request.clone();
        bob_signing.state = RequestState::Signing {
            approved_at: unix_now(),
            fee_sat: 300,
        };
        fx.store.save_agent_request("signet", &bob_signing).unwrap();
        list_reconciled(&fx.store, Network::Signet).unwrap();
        let bob_after = fx.request_of("bob", &bob_request.id);
        assert_eq!(
            bob_after.status(),
            "unresolved",
            "not settled by a partial match"
        );
        assert_eq!(bob_after.state.txid(), None);

        // Fully attributed to alice, the same record settles her.
        record = record.with_origin(TxOrigin {
            surface: "agent".into(),
            agent: Some("alice".into()),
            request_id: Some(alice_request.id.clone()),
            intent_digest: Some(alice_request.intent_digest.clone()),
        });
        fx.store.save_transaction("signet", &record).unwrap();
        // And alice's own interrupted record does settle from hers.
        let mut alice_signing = alice_request.clone();
        alice_signing.state = RequestState::Signing {
            approved_at: unix_now(),
            fee_sat: 300,
        };
        fx.store
            .save_agent_request("signet", &alice_signing)
            .unwrap();
        list_reconciled(&fx.store, Network::Signet).unwrap();
        let alice_after = fx.request_of("alice", &alice_request.id);
        assert_eq!(alice_after.status(), "broadcast_pending");
        assert_eq!(alice_after.state.txid(), Some(record.txid.as_str()));
        // Settling the broadcast is attribution-checked the same way.
        let mut tx = fx.store.load_transaction("signet", &record.txid).unwrap();
        tx.mark_broadcast();
        fx.store.save_transaction("signet", &tx).unwrap();
        crate::request::settle_broadcast(&fx.store, Network::Signet, &record.txid).unwrap();
        assert_eq!(fx.request_of("alice", &alice_request.id).status(), "sent");
        assert_eq!(fx.request_of("bob", &bob_request.id).status(), "unresolved");
    }

    // ---- boundaries at execution --------------------------------------

    /// The grant is re-read under the lock at commit: a boundary
    /// tightened between the review and the authorization refuses, with
    /// no signer constructed.
    #[test]
    fn commit_rechecks_the_grant_under_the_lock_before_signing() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        let Stage::Ready(staged) =
            stage(&fx.store, Network::Signet, || Ok(fx.services()), &id).unwrap()
        else {
            panic!("expected ready");
        };
        {
            let _lock = fx.store.lock_grants("signet").unwrap();
            let mut grant = fx.grant_state();
            grant.mode = GrantMode::Observe;
            fx.store.save_grant("signet", &grant).unwrap();
        }
        let probe = SignerProbe::default();
        let outcome = commit(&fx.store, Network::Signet, *staged, probe.factory()).unwrap();
        assert_eq!(outcome, Outcome::Denied(DenyReason::ObserveOnly));
        assert_eq!(probe.counts(), (0, 0));
        assert_eq!(fx.request(&id).status(), "denied");
        assert_eq!(fx.transactions(), 0);
        assert_eq!(fx.events(), vec!["request_received", "denied"]);
    }

    /// A cap tightened after filing refuses at staging, before any chain
    /// work: nothing is prepared, nothing is signed.
    #[test]
    fn cap_tightened_after_filing_is_denied_at_staging() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        let mut grant = fx.grant_state();
        grant.max_tx_sat = Some(9_999);
        fx.store.save_grant("signet", &grant).unwrap();
        let probe = SignerProbe::default();
        let outcome = approve(&fx, &id, &probe).unwrap();
        assert!(
            matches!(outcome, Outcome::Denied(DenyReason::OverMaxTx { .. })),
            "{outcome:?}"
        );
        assert_eq!(probe.counts(), (0, 0));
        let request = fx.request(&id);
        assert_eq!(request.status(), "denied");
        assert!(!request.is_approvable());
        assert_eq!(fx.events(), vec!["request_received", "denied"]);
    }

    #[test]
    fn dismiss_moves_a_pending_request_and_refuses_settled_ones() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        let dismissed = dismiss(&fx.store, Network::Signet, &id).unwrap();
        assert_eq!(dismissed.status(), "dismissed");
        assert_eq!(fx.events(), vec!["request_received", "dismissed"]);
        assert!(dismiss(&fx.store, Network::Signet, &id).is_err());
        let probe = SignerProbe::default();
        assert!(approve(&fx, &id, &probe).is_err());
        assert_eq!(probe.counts(), (0, 0));
    }

    /// Stale sync records a provably pre-signature failure for observation.
    #[test]
    fn sync_failure_records_a_safe_preparation_failure() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        std::fs::write(fx.mockdata.join("sync-error"), "offline").unwrap();
        let probe = SignerProbe::default();
        let err = approve(&fx, &id, &probe).unwrap_err();
        assert!(err.to_string().contains("stale state"), "{err:#}");
        assert_eq!(probe.counts(), (0, 0));
        let request = fx.request(&id);
        let RequestState::Failed { message, .. } = request.state else {
            panic!("expected failed");
        };
        assert!(message.contains("stale state"));
        assert!(message.contains("preparation failed before signing"));
        assert_eq!(fx.count_events("failed"), 1);
        std::fs::remove_file(fx.mockdata.join("sync-error")).unwrap();
        assert!(matches!(
            approve(&fx, &id, &probe).unwrap(),
            Outcome::Sent { .. }
        ));
        assert_eq!(probe.counts(), (1, 1));
        drop(fx.dir);
    }
    #[test]
    fn alkanes_carriers_unknown_outputs_and_indexer_failures_never_reach_the_signer() {
        for scenario in ["carrier", "unknown", "unavailable"] {
            let fx = Fixture::new(&[100_000]);
            let token = fx.grant();
            let id = fx.create(&token, "pay", 10_000).id;
            let path = fx.mockdata.join("alkanes-outpoints.json");
            if scenario == "unavailable" {
                std::fs::remove_file(path).unwrap();
            } else {
                let ctx = walletd::open(&fx.store, Network::Signet).unwrap();
                let answer = if scenario == "carrier" {
                    serde_json::json!({"32:0": "1000"})
                } else {
                    serde_json::Value::Null
                };
                let outputs: serde_json::Map<String, serde_json::Value> = ctx
                    .wallet
                    .list_unspent()
                    .map(|utxo| (utxo.outpoint.to_string(), answer.clone()))
                    .collect();
                std::fs::write(path, serde_json::to_vec(&outputs).unwrap()).unwrap();
            }
            let probe = SignerProbe::default();
            let err = approve(&fx, &id, &probe).unwrap_err();
            let expected = if scenario == "unavailable" {
                "alkanes view failed"
            } else {
                "all 1 unspent outputs are excluded"
            };
            assert!(format!("{err:#}").contains(expected), "{scenario}: {err:#}");
            assert_eq!(probe.counts(), (0, 0));
            assert_eq!(fx.request(&id).status(), "failed");
            assert_eq!(fx.transactions(), 0);
            assert!(!fx.mockdata.join("broadcasts.log").exists());
        }
    }

    #[test]
    fn fee_and_resolution_failures_are_observable_before_signing() {
        for failure in ["fee", "resolution"] {
            let fx = Fixture::new(&[100_000]);
            let token = fx.grant();
            let id = fx.create(&token, "pay", 10_000).id;
            if failure == "fee" {
                std::fs::write(fx.mockdata.join("fees.json"), "invalid").unwrap();
            }
            assert!(
                stage(
                    &fx.store,
                    Network::Signet,
                    || {
                        if failure == "resolution" {
                            bail!("provider unavailable");
                        }
                        Ok(fx.services())
                    },
                    &id
                )
                .is_err()
            );
            let record = fx.request(&id);
            assert!(record.is_approvable());
            let RequestState::Failed { message, .. } = record.state else {
                panic!("expected failed");
            };
            assert!(message.contains("preparation failed before signing"));
            assert_eq!(fx.transactions(), 0);
            assert_eq!(fx.count_events("failed"), 1);
        }
    }

    #[test]
    fn preparation_failure_never_overwrites_concurrent_settled_or_uncertain_state() {
        for state in [
            RequestState::Dismissed { at: 2 },
            RequestState::Signing {
                approved_at: 2,
                fee_sat: 100,
            },
            RequestState::Unresolved {
                at: 2,
                message: "uncertain".into(),
                txid: None,
            },
            RequestState::Sent {
                at: 2,
                fee_sat: 100,
                txid: "saved".into(),
            },
        ] {
            let fx = Fixture::new(&[100_000]);
            let token = fx.grant();
            let id = fx.create(&token, "pay", 10_000).id;
            assert!(
                stage(
                    &fx.store,
                    Network::Signet,
                    || {
                        let _lock = fx.store.lock_grants("signet").unwrap();
                        let mut record = fx.request(&id);
                        record.state = state.clone();
                        fx.store.save_agent_request("signet", &record).unwrap();
                        bail!("preparation interrupted")
                    },
                    &id
                )
                .is_err()
            );
            assert_eq!(fx.request(&id).state, state);
            assert_eq!(fx.count_events("failed"), 0);
        }
    }
    #[test]
    fn preparation_failure_preserves_replaced_or_unsupported_records() {
        for replacement in ["version", "grant", "intent"] {
            let fx = Fixture::new(&[100_000]);
            let token = fx.grant();
            let id = fx.create(&token, "pay", 10_000).id;
            let err = stage(
                &fx.store,
                Network::Signet,
                || {
                    let _lock = fx.store.lock_grants("signet").unwrap();
                    let mut record = fx.request(&id);
                    match replacement {
                        "version" => record.format_version += 1,
                        "grant" => record.grant_id = "replacement".into(),
                        _ => record.intent_digest = "replacement".into(),
                    }
                    fx.store.save_agent_request("signet", &record).unwrap();
                    bail!("preparation interrupted")
                },
                &id,
            )
            .err()
            .expect("must fail");
            assert!(format!("{err:#}").contains("request changed during preparation"));
            assert_eq!(fx.request(&id).status(), "pending_approval");
            assert_eq!(fx.count_events("failed"), 0);
        }
    }
    #[test]
    fn broadcast_receipt_write_failure_recovers_repeatedly_without_resigning() {
        for via_listing in [false, true] {
            let fx = Fixture::new(&[100_000]);
            let token = fx.grant();
            let id = fx.create(&token, "pay", 10_000).id;
            let probe = SignerProbe::default();
            std::fs::write(fx.mockdata.join("broadcast-fail"), "response lost").unwrap();
            let Outcome::BroadcastPending { txid, .. } = approve(&fx, &id, &probe).unwrap() else {
                panic!("expected broadcast_pending");
            };
            let before = fx.store.load_transaction("signet", &txid).unwrap();
            std::fs::remove_file(fx.mockdata.join("broadcast-fail")).unwrap();
            // Block only the atomic receipt write, after successful broadcast.
            let blocker = fx
                .store
                .agent_requests_dir("signet")
                .join("claude")
                .join(format!("{id}.tmp"));
            std::fs::create_dir(&blocker).unwrap();
            crate::spend::rebroadcast(&fx.store, Network::Signet, || Ok(fx.services()), &txid)
                .unwrap();
            assert_eq!(fx.request(&id).status(), "broadcast_pending");
            let saved = fx.store.load_transaction("signet", &txid).unwrap();
            assert_eq!(saved.status, TransactionStatus::Broadcast);
            assert_eq!(saved.tx_hex, before.tx_hex);
            std::fs::remove_dir(&blocker).unwrap();
            for _ in 0..2 {
                if via_listing {
                    list_reconciled(&fx.store, Network::Signet).unwrap();
                } else {
                    crate::spend::rebroadcast(
                        &fx.store,
                        Network::Signet,
                        || panic!("receipt repair must not resolve providers"),
                        &txid,
                    )
                    .unwrap();
                }
                assert_eq!(fx.request(&id).status(), "sent");
                assert_eq!(fx.transactions(), 1);
                assert_eq!(probe.counts(), (1, 1));
                assert_eq!(fx.count_events("broadcast"), 1);
            }
            assert_eq!(
                std::fs::read_to_string(fx.mockdata.join("broadcasts.log"))
                    .unwrap()
                    .lines()
                    .count(),
                1
            );
            assert!(approve(&fx, &id, &probe).is_err());
        }
    }

    #[test]
    fn receipt_recovery_requires_broadcast_status_and_the_exact_transaction() {
        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        let probe = SignerProbe::default();
        std::fs::write(fx.mockdata.join("broadcast-fail"), "response lost").unwrap();
        let Outcome::BroadcastPending { txid, .. } = approve(&fx, &id, &probe).unwrap() else {
            panic!("expected pending");
        };
        crate::request::settle_broadcast(&fx.store, Network::Signet, &txid).unwrap();
        assert_eq!(fx.request(&id).status(), "broadcast_pending");
        let mut record = fx.store.load_transaction("signet", &txid).unwrap();
        record.mark_broadcast();
        fx.store.save_transaction("signet", &record).unwrap();
        let mut request = fx.request(&id);
        request.state = RequestState::BroadcastPending {
            txid: "wrong-transaction".into(),
            fee_sat: record.fee_sat,
            at: 1,
        };
        fx.store.save_agent_request("signet", &request).unwrap();
        crate::request::settle_broadcast(&fx.store, Network::Signet, &txid).unwrap();
        list_reconciled(&fx.store, Network::Signet).unwrap();
        assert_eq!(fx.request(&id).state, request.state);
        assert_eq!(fx.count_events("broadcast"), 0);
        request.state = RequestState::Signing {
            approved_at: 1,
            fee_sat: record.fee_sat,
        };
        fx.store.save_agent_request("signet", &request).unwrap();
        let claim = fx
            .store
            .claim_agent_request("signet", "claude", &id)
            .unwrap()
            .unwrap();
        assert!(crate::request::settle_broadcast(&fx.store, Network::Signet, &txid).is_err());
        assert_eq!(fx.request(&id).state, request.state);
        drop(claim);
        // A file at the expected path is insufficient: embedded identity must match.
        let path = fx
            .store
            .agent_requests_dir("signet")
            .join("claude")
            .join(format!("{id}.json"));
        let mut wrong_identity = request.clone();
        wrong_identity.agent = "other".into();
        std::fs::write(&path, serde_json::to_vec(&wrong_identity).unwrap()).unwrap();
        assert!(crate::request::settle_broadcast(&fx.store, Network::Signet, &txid).is_err());
        assert_eq!(fx.request(&id).agent, "other");
        std::fs::write(path, serde_json::to_vec(&request).unwrap()).unwrap();
        crate::request::settle_broadcast(&fx.store, Network::Signet, &txid).unwrap();
        assert_eq!(fx.request(&id).status(), "sent");
    }

    #[test]
    fn lost_broadcast_response_recovers_the_exact_bytes_with_one_signature() {
        use std::io::{BufRead, BufReader, Read};
        use std::net::TcpListener;
        use std::time::{Duration, Instant};

        let fx = Fixture::new(&[100_000]);
        let token = fx.grant();
        let id = fx.create(&token, "pay", 10_000).id;
        let probe = SignerProbe::default();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "broadcast never reached fixture");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("{err}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.starts_with("POST /tx "));
            let mut length = 0;
            loop {
                line.clear();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes).unwrap();
            // The fixture accepted these bytes, then lost the response.
            String::from_utf8(bytes).unwrap()
        });
        // Mock sync and fees; broadcast through the real Esplora transport.
        let broadcast_via = fx.mockdata.join("broadcast-via");
        std::fs::write(&broadcast_via, &url).unwrap();
        let Outcome::BroadcastPending { txid, .. } = approve(&fx, &id, &probe).unwrap() else {
            panic!("response must be uncertain");
        };
        let received_hex = server.join().unwrap();
        let saved = fx.store.load_transaction("signet", &txid).unwrap();
        assert_eq!(saved.tx_hex, received_hex);
        assert_eq!(saved.status, TransactionStatus::Pending);
        std::fs::remove_file(&broadcast_via).unwrap();
        crate::spend::rebroadcast(&fx.store, Network::Signet, || Ok(fx.services()), &txid).unwrap();
        crate::spend::rebroadcast(
            &fx.store,
            Network::Signet,
            || panic!("already broadcast"),
            &txid,
        )
        .unwrap();
        assert_eq!(
            fx.store.load_transaction("signet", &txid).unwrap().tx_hex,
            received_hex
        );
        assert_eq!(fx.request(&id).status(), "sent");
        assert_eq!(probe.counts(), (1, 1));
        assert_eq!(fx.transactions(), 1);
    }
}
