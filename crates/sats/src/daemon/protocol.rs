//! The satsd wire protocol: newline-delimited JSON over a unix socket.
//!
//! Framing is one JSON object per line in each direction, matching the
//! event log's format so a session can be read with ordinary tools while
//! developing. The socket is local and owner-only; nothing here is
//! reachable over a network.
//!
//! A send spans two calls on **one connection**: `BeginSend` claims the
//! request, `Authorize` signs it. The claim is released when the
//! connection closes, so a caller that dies mid-send never strands it.

use std::io::{BufRead, Write};

use anyhow::{Context, Result, bail};
use sats_core::authz::DenyReason;
use serde::{Deserialize, Serialize};

/// Incompatible changes bump this; the daemon refuses mismatches rather
/// than guessing at an older client's intent.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest line either side will read. A send carries a base64 PSBT, so
/// the bound is generous, but it is a bound.
const MAX_LINE: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Liveness, lock state, and grant count. No token required: it
    /// reveals nothing an owner of the data directory cannot already read.
    Status { protocol: u32 },
    /// Human authorization. The password unseals the master seed into the
    /// daemon's memory; it is never stored and never leaves this call.
    Unlock { protocol: u32, password: String },
    /// Request a human password dialog. No password crosses this request
    /// or its response; only the daemon owns the prompt helper.
    #[serde(rename = "request_unlock")]
    PromptUnlock {
        protocol: u32,
        token: String,
        agent: String,
    },
    /// Zeroize the in-memory seed. The daemon keeps running and keeps
    /// refusing every signature until it is unlocked again.
    Lock { protocol: u32 },
    /// Claim a send request and run the amount-only precheck.
    BeginSend {
        protocol: u32,
        token: String,
        agent: String,
        request_id: Option<String>,
        recipient: String,
        amount_sat: u64,
    },
    /// Authorize and sign the prepared PSBT claimed by `BeginSend` on
    /// this connection. The daemon derives the amount and fee from the
    /// PSBT itself; nothing about the transaction is taken on trust.
    Authorize {
        protocol: u32,
        token: String,
        psbt: String,
        /// Display-only count of outputs the caller's guards excluded.
        /// It reaches the transaction record and nothing else; no
        /// decision reads it, so a wrong value cannot authorize anything.
        excluded_utxos: u64,
    },
    /// Stop the daemon. The seed goes with the process.
    Shutdown { protocol: u32 },
    /// Report what happened to the signed transaction, so the request
    /// record and event log close out.
    Finish {
        protocol: u32,
        token: String,
        outcome: BroadcastOutcome,
    },
}

impl Request {
    pub fn protocol(&self) -> u32 {
        match self {
            Request::Status { protocol }
            | Request::Unlock { protocol, .. }
            | Request::PromptUnlock { protocol, .. }
            | Request::Lock { protocol }
            | Request::BeginSend { protocol, .. }
            | Request::Authorize { protocol, .. }
            | Request::Shutdown { protocol }
            | Request::Finish { protocol, .. } => *protocol,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "broadcast", rename_all = "snake_case")]
pub enum BroadcastOutcome {
    Broadcast { txid: String },
    Failed { message: String },
}

/// Tagged `reply`, not `status`: `Outcome` is a newtype variant, so its
/// fields flatten into the same object, and `SendOutcome` already has a
/// `status` of its own.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Response {
    Status(StatusInfo),
    Unlock(super::unlock::UnlockResult),
    Ok,
    /// The request is claimed and passed the precheck. Prepare the
    /// transaction, then send `Authorize` on this same connection.
    Proceed {
        request_id: String,
    },
    /// A terminal send result: sent, denied, or a typed error.
    Outcome(SendOutcome),
    /// A connection-level failure: bad protocol version, unknown token,
    /// or a call out of sequence.
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusInfo {
    pub protocol: u32,
    pub version: String,
    pub network: String,
    pub locked: bool,
    pub grants: usize,
    /// Seconds until the idle auto-lock fires; absent while locked.
    pub locks_in: Option<u64>,
}

/// The result vocabulary shared by the daemon and every surface that
/// reports a send. Field names and denial codes are a stable contract:
/// `mcp::SendResult` is this shape with a JSON schema attached.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SendOutcome {
    /// "sent", "denied", or "error".
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub txid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount_sat: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_sat: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_sat: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_budget_sat: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via_approval: Option<bool>,
    /// On denied outcomes only: whether a one-time human approval can
    /// lift this exact refusal. False marks the hard envelope, where the
    /// only escalation is changing the grant itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approvable: Option<bool>,
}

impl SendOutcome {
    pub fn sent(txid: String, amount_sat: u64, fee_sat: u64, remaining: Option<u64>) -> Self {
        SendOutcome {
            status: "sent".into(),
            txid: Some(txid),
            amount_sat: Some(amount_sat),
            fee_sat: Some(fee_sat),
            total_sat: Some(amount_sat.saturating_add(fee_sat)),
            remaining_budget_sat: remaining,
            ..Default::default()
        }
    }

    pub fn denied(reason: &str, message: String) -> Self {
        SendOutcome {
            status: "denied".into(),
            reason: Some(reason.into()),
            message: Some(message),
            // Denials built from a bare code — revocation, no grant on
            // file — have no approval path: there is nothing to approve
            // against. Typed refusals override this via `from_deny`.
            approvable: Some(false),
            ..Default::default()
        }
    }

    /// The denial shape shared by every refusal path.
    pub fn from_deny(reason: &DenyReason) -> Self {
        SendOutcome {
            approvable: Some(reason.approvable()),
            ..SendOutcome::denied(
                reason.code(),
                format!(
                    "human authorization required: {}",
                    reason.human().replace('\n', "; ")
                ),
            )
        }
    }

    pub fn error(message: String) -> Self {
        SendOutcome {
            status: "error".into(),
            message: Some(message),
            ..Default::default()
        }
    }

    /// An operational error the caller can branch on mechanically.
    pub fn op_error(code: &str, message: String) -> Self {
        SendOutcome {
            error_code: Some(code.into()),
            ..SendOutcome::error(message)
        }
    }

    pub fn with_request(mut self, id: &str) -> Self {
        self.request_id = Some(id.into());
        self
    }

    pub fn is_sent(&self) -> bool {
        self.status == "sent"
    }
}

/// Write one framed message.
pub fn write_message<W: Write, T: Serialize>(out: &mut W, message: &T) -> Result<()> {
    let mut line = zeroize::Zeroizing::new(serde_json::to_vec(message)?);
    line.push(b'\n');
    out.write_all(&line)?;
    out.flush()?;
    Ok(())
}

/// Read one framed message. `Ok(None)` means the peer closed cleanly.
///
/// Bounded: a peer that never sends a newline hits [`MAX_LINE`] and is
/// refused rather than growing this process's memory without limit.
pub fn read_message<R: BufRead, T: for<'de> Deserialize<'de>>(input: &mut R) -> Result<Option<T>> {
    let mut buf = zeroize::Zeroizing::new(Vec::new());
    // Call-syntax, not method-syntax: `input.take(..)` would resolve to
    // `R::take` and move the caller's reader instead of borrowing it.
    let mut limited = std::io::Read::take(&mut *input, MAX_LINE);
    let read = limited
        .read_until(b'\n', &mut buf)
        .context("cannot read from socket")?;
    if read == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        bail!("message exceeds {MAX_LINE} bytes or was truncated");
    }
    buf.pop();
    let line = std::str::from_utf8(&buf).context("message is not valid utf-8")?;
    Ok(Some(serde_json::from_str(line).context(
        "malformed message: the peer is not speaking the satsd protocol",
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_framed() {
        let mut buf: Vec<u8> = Vec::new();
        let request = Request::BeginSend {
            protocol: PROTOCOL_VERSION,
            token: "aa".repeat(32),
            agent: "claude".into(),
            request_id: Some("k-1".into()),
            recipient: "tb1p".into(),
            amount_sat: 25_000,
        };
        write_message(&mut buf, &request).unwrap();
        write_message(&mut buf, &Response::Ok).unwrap();
        assert_eq!(buf.iter().filter(|b| **b == b'\n').count(), 2);

        let mut cursor = std::io::Cursor::new(buf);
        let back: Request = read_message(&mut cursor).unwrap().unwrap();
        assert!(matches!(
            back,
            Request::BeginSend {
                amount_sat: 25_000,
                ..
            }
        ));
        assert_eq!(back.protocol(), PROTOCOL_VERSION);
        let back: Response = read_message(&mut cursor).unwrap().unwrap();
        assert!(matches!(back, Response::Ok));
        let end: Option<Response> = read_message(&mut cursor).unwrap();
        assert!(end.is_none(), "clean close reads as None");
    }

    #[test]
    fn a_truncated_line_is_an_error_not_a_partial_message() {
        let mut cursor = std::io::Cursor::new(b"{\"op\":\"lock\",\"protocol\":1}".to_vec());
        let result: Result<Option<Request>> = read_message(&mut cursor);
        assert!(result.is_err(), "an unterminated line must not parse");
    }

    #[test]
    fn garbage_is_refused_with_a_useful_message() {
        let mut cursor = std::io::Cursor::new(b"not json\n".to_vec());
        let err = read_message::<_, Request>(&mut cursor).unwrap_err();
        assert!(format!("{err:#}").contains("satsd protocol"));
    }

    /// Every response shape must survive the wire. `Outcome` is the one
    /// that can collide: it flattens a struct that has its own `status`.
    #[test]
    fn every_response_variant_round_trips() {
        let responses = vec![
            Response::Ok,
            Response::Unlock(super::super::unlock::UnlockResult::cancelled()),
            Response::Proceed {
                request_id: "k-1".into(),
            },
            Response::Status(StatusInfo {
                protocol: PROTOCOL_VERSION,
                version: "0.1.0".into(),
                network: "signet".into(),
                locked: false,
                grants: 2,
                locks_in: Some(3600),
            }),
            Response::Outcome(SendOutcome::denied("over_max_tx", "nope".into())),
            Response::Outcome(SendOutcome::sent("ab".into(), 100, 20, Some(500))),
            Response::Outcome(SendOutcome::op_error("wallet_locked", "locked".into())),
            Response::Error {
                code: "out_of_sequence".into(),
                message: "no".into(),
            },
        ];
        let mut buf: Vec<u8> = Vec::new();
        for response in &responses {
            write_message(&mut buf, response).expect("serializable");
        }
        let mut cursor = std::io::Cursor::new(buf);
        for expected in &responses {
            let back: Response = read_message(&mut cursor)
                .expect("readable")
                .expect("present");
            assert_eq!(
                serde_json::to_value(&back).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
    }

    #[test]
    fn every_request_variant_round_trips() {
        let requests = vec![
            Request::PromptUnlock {
                protocol: PROTOCOL_VERSION,
                agent: "claude".into(),
                token: "aa".repeat(32),
            },
            Request::Status {
                protocol: PROTOCOL_VERSION,
            },
            Request::Unlock {
                protocol: PROTOCOL_VERSION,
                password: "pw".into(),
            },
            Request::Lock {
                protocol: PROTOCOL_VERSION,
            },
            Request::Shutdown {
                protocol: PROTOCOL_VERSION,
            },
            Request::BeginSend {
                protocol: PROTOCOL_VERSION,
                token: "aa".repeat(32),
                agent: "claude".into(),
                request_id: None,
                recipient: "tb1p".into(),
                amount_sat: 1,
            },
            Request::Authorize {
                protocol: PROTOCOL_VERSION,
                token: "aa".repeat(32),
                psbt: "cHNidP8".into(),
                excluded_utxos: 0,
            },
            Request::Finish {
                protocol: PROTOCOL_VERSION,
                token: "aa".repeat(32),
                outcome: BroadcastOutcome::Broadcast { txid: "ab".into() },
            },
            Request::Finish {
                protocol: PROTOCOL_VERSION,
                token: "aa".repeat(32),
                outcome: BroadcastOutcome::Failed {
                    message: "no route".into(),
                },
            },
        ];
        let mut buf: Vec<u8> = Vec::new();
        for request in &requests {
            write_message(&mut buf, request).expect("serializable");
        }
        let mut cursor = std::io::Cursor::new(buf);
        for expected in &requests {
            let back: Request = read_message(&mut cursor)
                .expect("readable")
                .expect("present");
            assert_eq!(
                serde_json::to_value(&back).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
    }

    #[test]
    fn outcomes_omit_absent_fields() {
        let json = serde_json::to_value(SendOutcome::denied("over_budget", "no".into())).unwrap();
        assert_eq!(json["status"], "denied");
        assert_eq!(json["reason"], "over_budget");
        assert!(json.get("txid").is_none());
        assert!(json.get("tx_hex").is_none());

        let sent = SendOutcome::sent("ab".into(), 100, 20, Some(500));
        let json = serde_json::to_value(&sent).unwrap();
        assert_eq!(json["total_sat"], 120);
        assert_eq!(json["remaining_budget_sat"], 500);
        assert!(sent.is_sent());
    }

    /// `approvable` rides denied outcomes only, and tracks the typed
    /// reason: the ask band true, the hard envelope false, bare-code
    /// denials (revoked) false.
    #[test]
    fn approvable_marks_denials_and_nothing_else() {
        let ask = SendOutcome::from_deny(&DenyReason::OverMaxTx {
            requested_sat: 2,
            max_tx_sat: 1,
        });
        assert_eq!(serde_json::to_value(&ask).unwrap()["approvable"], true);

        let hard = SendOutcome::from_deny(&DenyReason::OverAskMax {
            requested_sat: 2,
            ask_max_tx_sat: 1,
        });
        assert_eq!(serde_json::to_value(&hard).unwrap()["approvable"], false);

        let revoked = SendOutcome::denied("revoked", "no grant".into());
        assert_eq!(serde_json::to_value(&revoked).unwrap()["approvable"], false);

        let sent = serde_json::to_value(SendOutcome::sent("ab".into(), 1, 1, None)).unwrap();
        assert!(sent.get("approvable").is_none());
        let error =
            serde_json::to_value(SendOutcome::op_error("wallet_locked", "l".into())).unwrap();
        assert!(error.get("approvable").is_none());
    }
}
