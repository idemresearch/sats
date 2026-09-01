//! The satsd listener.
//!
//! One daemon serves one network. It holds the master seed in memory and
//! is the only thing in the system that can produce a signature for an
//! agent. It has **no chain access**: callers sync, estimate fees, apply
//! guards, and broadcast; the daemon only decides and signs.
//!
//! A connection is a session. The request claim taken by `BeginSend`
//! lives on the connection, so a caller that dies between preparing and
//! authorizing releases it without leaving anything locked.

use std::io::{BufReader, ErrorKind, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sats_core::bitcoin::Network;

use crate::config::network_name;
use crate::daemon::protocol::{self, PROTOCOL_VERSION, Request, Response, SendOutcome, StatusInfo};
use crate::daemon::send::{self, Begin, InFlight};
use crate::daemon::session::{Session, UnlockError};
use crate::store::{Store, unix_now};

/// How often the idle sweep runs. The auto-lock deadline is exact to
/// within this interval.
const SWEEP: Duration = Duration::from_secs(15);

/// Serve until the process is stopped.
pub fn run(store: Store, network: Network, auto_lock_after: Duration) -> Result<()> {
    let net_name = network_name(network);
    let path = store.socket_path(net_name);

    // Hold a stable inode for the process lifetime, before touching the socket.
    // Never unlink this lock: another contender may already have it open.
    let _lifetime_lock = crate::store::lock_daemon(&path)?;

    // A live daemon on this socket is a conflict; a dead one is litter.
    if path.exists() {
        if super::client::Client::connect(&path).is_ok() {
            bail!(
                "satsd is already running for {net_name} ({})",
                path.display()
            );
        }
        std::fs::remove_file(&path)
            .with_context(|| format!("cannot clear stale socket {}", path.display()))?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        crate::store::harden_dir(parent)?;
    }
    let listener =
        UnixListener::bind(&path).with_context(|| format!("cannot bind {}", path.display()))?;
    crate::store::harden_file(&path)?;

    let store = Arc::new(store);
    let session = Arc::new(Session::new(network, net_name, auto_lock_after));

    {
        let session = Arc::clone(&session);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(SWEEP);
                if session.lock_if_idle() {
                    eprintln!("satsd: locked after {}s idle", auto_lock_after.as_secs());
                }
            }
        });
    }

    eprintln!(
        "satsd: serving {net_name} on {} — locked, run `sats daemon unlock` to enable signing",
        path.display()
    );

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("satsd: accept failed: {err}");
                continue;
            }
        };
        let store = Arc::clone(&store);
        let session = Arc::clone(&session);
        std::thread::spawn(move || {
            if let Err(err) = serve_connection(&store, &session, stream) {
                eprintln!("satsd: connection ended: {err:#}");
            }
        });
    }
    Ok(())
}

/// Handle one connection until the peer closes it.
fn serve_connection(store: &Store, session: &Session, stream: UnixStream) -> Result<()> {
    let mut writer = stream.try_clone().context("cannot split socket")?;
    let mut reader = BufReader::new(stream);
    // The claimed request for this connection's send, if one is running.
    let mut flight: Option<Box<InFlight>> = None;

    while let Some(request) = protocol::read_message::<_, Request>(&mut reader)? {
        if request.protocol() != PROTOCOL_VERSION {
            let response = Response::Error {
                code: "protocol_mismatch".into(),
                message: format!(
                    "satsd speaks protocol {PROTOCOL_VERSION}, the caller speaks {} — \
                     both sides must be the same sats version",
                    request.protocol()
                ),
            };
            protocol::write_message(&mut writer, &response)?;
            continue;
        }
        // Shutdown answers before it acts, so the caller sees success
        // rather than a closed socket.
        if let Request::Shutdown { .. } = request {
            protocol::write_message(&mut writer, &Response::Ok)?;
            let _ = session.lock();
            let _ = std::fs::remove_file(store.socket_path(session.net_name));
            eprintln!("satsd: stopped");
            std::process::exit(0);
        }
        if let Request::PromptUnlock { agent, token, .. } = request {
            if flight.is_some() {
                protocol::write_message(
                    &mut writer,
                    &out_of_sequence("a send is already in progress"),
                )?;
                continue;
            }
            let token = zeroize::Zeroizing::new(token);
            // Dedicated connection: any extra bytes, EOF, or read failure
            // cancels consent. A tiny read timeout lets the helper poll
            // without requiring nonportable socket-peek APIs.
            reader
                .get_ref()
                .set_read_timeout(Some(Duration::from_millis(1)))?;
            let result = super::unlock::request(
                store,
                session,
                &agent,
                &token,
                || matches!(reader.read(&mut [0; 1]), Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted)),
            );
            protocol::write_message(&mut writer, &Response::Unlock(result))?;
            return Ok(());
        }
        let response = dispatch(store, session, &mut flight, request);
        protocol::write_message(&mut writer, &response)?;
    }
    Ok(())
}

fn dispatch(
    store: &Store,
    session: &Session,
    flight: &mut Option<Box<InFlight>>,
    request: Request,
) -> Response {
    match request {
        Request::Status { .. } => Response::Status(StatusInfo {
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
            network: session.net_name.to_string(),
            locked: session.is_locked(),
            grants: store
                .active_grants(session.net_name, unix_now())
                .map(|g| g.len())
                .unwrap_or(0),
            locks_in: session.locks_in(),
        }),

        Request::Unlock { password, .. } => {
            match session.unlock(store, &zeroize::Zeroizing::new(password)) {
                Ok(()) => Response::Ok,
                // A throttled attempt is a distinct, typed condition: the
                // password was not even tried, and retrying sooner cannot help.
                Err(err @ UnlockError::Throttled { .. }) => Response::Error {
                    code: "unlock_throttled".into(),
                    message: err.to_string(),
                },
                Err(err) => Response::Error {
                    code: "unlock_failed".into(),
                    message: err.to_string(),
                },
            }
        }

        Request::Lock { .. } => match session.lock() {
            Ok(()) => Response::Ok,
            Err(err) => Response::Error {
                code: "lock_failed".into(),
                message: format!("{err:#}"),
            },
        },

        Request::BeginSend {
            token,
            agent,
            request_id,
            recipient,
            amount_sat,
            ..
        } => {
            if flight.is_some() {
                return out_of_sequence("a send is already in progress on this connection");
            }
            // Locked is not a policy denial: an agent must be able to tell
            // "your budget said no" from "no human has unlocked the wallet".
            if session.is_locked() {
                return Response::Outcome(locked_outcome());
            }
            match send::begin(
                store,
                session.net_name,
                &agent,
                &token,
                request_id.as_deref(),
                &recipient,
                amount_sat,
            ) {
                Begin::Proceed(claimed) => {
                    let id = claimed.request.id.clone();
                    *flight = Some(claimed);
                    Response::Proceed { request_id: id }
                }
                Begin::Done(outcome) => Response::Outcome(*outcome),
            }
        }

        Request::Authorize {
            token,
            psbt,
            excluded_utxos,
            ..
        } => {
            let Some(claimed) = flight.as_mut() else {
                return out_of_sequence("authorize without a claimed request");
            };
            if claimed.signed.is_some() {
                return out_of_sequence("this request is already signed");
            }
            let key = match session.signing_key() {
                Ok(key) => key,
                Err(_) => return Response::Outcome(locked_outcome()),
            };
            let outcome = send::authorize(
                store,
                &key,
                session.net_name,
                &token,
                claimed,
                &psbt,
                excluded_utxos,
                || {
                    Ok(Box::new(sats_core::signer::LocalSigner::new(
                        key.mnemonic()?,
                        key.network,
                    )))
                },
            );
            // Anything other than a signature ends the send: drop the
            // claim so a retry can re-evaluate under the same id.
            if !outcome.is_sent() {
                *flight = None;
            }
            Response::Outcome(outcome)
        }

        // Handled before dispatch so it can answer and then exit.
        Request::Shutdown { .. } => Response::Ok,
        Request::PromptUnlock { .. } => {
            out_of_sequence("unlock prompts require a dedicated connection")
        }

        Request::Finish { token, outcome, .. } => {
            let Some(claimed) = flight.as_mut() else {
                return out_of_sequence("finish without a signed request");
            };
            // Only the token that began this send may close it out. The
            // comparison is against the Begin-time hash, not a grant
            // re-read, so a send signed just before a revocation can still
            // record its broadcast outcome. Refuse without recording and
            // keep the claim: the same connection can retry with the
            // right token.
            if !claimed.authorizes(&token) {
                return Response::Outcome(
                    SendOutcome::op_error(
                        "unauthorized",
                        "the presented token is not the one that began this send — \
                         the outcome was not recorded"
                            .into(),
                    )
                    .with_request(&claimed.request.id),
                );
            }
            let result = send::finish(store, session.net_name, claimed, outcome);
            *flight = None;
            Response::Outcome(result)
        }
    }
}

fn locked_outcome() -> SendOutcome {
    SendOutcome::op_error(
        "wallet_locked",
        "the wallet is locked — a human must run: sats daemon unlock".into(),
    )
}

fn out_of_sequence(what: &str) -> Response {
    Response::Error {
        code: "out_of_sequence".into(),
        message: format!("protocol error: {what}"),
    }
}
