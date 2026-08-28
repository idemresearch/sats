//! Talking to satsd.
//!
//! Used by the `sats daemon` commands and by the MCP server, which is a
//! shim over this client: it prepares transactions and broadcasts them,
//! and holds nothing that could produce a signature on its own.

use std::io::{BufReader, Read};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::daemon::protocol::{
    self, BroadcastOutcome, PROTOCOL_VERSION, Request, Response, SendOutcome, StatusInfo,
};
use crate::store::Store;

pub struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

/// Dropping a cancelled MCP request shuts down its daemon connection even
/// while the blocking worker still owns another descriptor for that socket.
#[cfg(feature = "mcp")]
pub struct DisconnectOnDrop(UnixStream);

#[cfg(feature = "mcp")]
impl Drop for DisconnectOnDrop {
    fn drop(&mut self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

/// What `BeginSend` resolved to.
pub enum Begun {
    /// Claimed. Prepare the transaction, then call [`Client::authorize`]
    /// on this same client — the claim lives on the connection.
    Proceed { request_id: String },
    /// The daemon answered without needing a prepared transaction.
    Done(SendOutcome),
}

impl Client {
    #[cfg(feature = "mcp")]
    pub fn disconnect_on_drop(&self) -> Result<DisconnectOnDrop> {
        Ok(DisconnectOnDrop(self.writer.try_clone()?))
    }

    #[cfg(feature = "mcp")]
    pub fn request_unlock(
        &mut self,
        agent: &str,
        token: &str,
    ) -> Result<super::unlock::UnlockResult> {
        self.writer
            .set_write_timeout(Some(Duration::from_secs(2)))?;
        self.reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(130)))?;
        match self.call(Request::PromptUnlock {
            protocol: PROTOCOL_VERSION,
            agent: agent.into(),
            token: token.into(),
        })? {
            Response::Unlock(result) => Ok(result),
            // Do not forward arbitrary replies or diagnostics into the model.
            _ => bail!(
                "daemon does not support unlock prompts; restart it with the updated sats binary"
            ),
        }
    }

    /// A liveness probe must not hang MCP discovery on an unresponsive daemon.
    /// Only this connection has short deadlines; signing calls are unaffected.
    pub fn probe(store: &Store, net_name: &str) -> Result<StatusInfo> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut stream = UnixStream::connect(store.socket_path(net_name))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        protocol::write_message(
            &mut stream,
            &Request::Status {
                protocol: PROTOCOL_VERSION,
            },
        )?;
        let mut bytes = Vec::new();
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .context("satsd status probe timed out")?;
            stream.set_read_timeout(Some(remaining))?;
            let mut buffer = [0; 4096];
            let read = stream
                .read(&mut buffer)
                .context("satsd status probe failed")?;
            if read == 0 {
                bail!("satsd closed the status connection");
            }
            bytes.extend_from_slice(&buffer[..read]);
            if bytes.len() > 4 * 1024 * 1024 {
                bail!("satsd status response too large");
            }
            if let Some(end) = bytes.iter().position(|&b| b == b'\n') {
                return match serde_json::from_slice::<Response>(&bytes[..end])? {
                    Response::Status(info) => Ok(info),
                    Response::Error { code, message } => bail!("{message} [{code}]"),
                    other => bail!("unexpected reply from satsd: {}", label(&other)),
                };
            }
        }
    }

    pub fn connect(path: &Path) -> Result<Client> {
        let stream = UnixStream::connect(path)
            .with_context(|| format!("cannot connect to {}", path.display()))?;
        Ok(Client {
            writer: stream.try_clone().context("cannot split socket")?,
            reader: BufReader::new(stream),
        })
    }

    /// Connect to the daemon for a network, with the remedy in the error.
    pub fn open(store: &Store, net_name: &str) -> Result<Client> {
        let path = store.socket_path(net_name);
        Client::connect(&path).with_context(|| {
            format!(
                "satsd is not running for {net_name} — start it with: sats daemon start \
                 --network {net_name}"
            )
        })
    }

    fn call(&mut self, request: Request) -> Result<Response> {
        protocol::write_message(&mut self.writer, &request)?;
        protocol::read_message(&mut self.reader)?
            .context("satsd closed the connection without answering")
    }

    /// A call whose only non-error answer is `Ok`.
    fn call_ok(&mut self, request: Request) -> Result<()> {
        match self.call(request)? {
            Response::Ok => Ok(()),
            Response::Error { code, message } => bail!("{message} [{code}]"),
            other => bail!("unexpected reply from satsd: {}", label(&other)),
        }
    }

    pub fn status(&mut self) -> Result<StatusInfo> {
        match self.call(Request::Status {
            protocol: PROTOCOL_VERSION,
        })? {
            Response::Status(info) => Ok(info),
            Response::Error { code, message } => bail!("{message} [{code}]"),
            other => bail!("unexpected reply from satsd: {}", label(&other)),
        }
    }

    pub fn unlock(&mut self, password: &str) -> Result<()> {
        self.call_ok(Request::Unlock {
            protocol: PROTOCOL_VERSION,
            password: password.to_string(),
        })
    }

    pub fn lock(&mut self) -> Result<()> {
        self.call_ok(Request::Lock {
            protocol: PROTOCOL_VERSION,
        })
    }

    pub fn shutdown(&mut self) -> Result<()> {
        self.call_ok(Request::Shutdown {
            protocol: PROTOCOL_VERSION,
        })
    }

    pub fn begin_send(
        &mut self,
        token: &str,
        agent: &str,
        request_id: Option<&str>,
        recipient: &str,
        amount_sat: u64,
    ) -> Result<Begun> {
        match self.call(Request::BeginSend {
            protocol: PROTOCOL_VERSION,
            token: token.to_string(),
            agent: agent.to_string(),
            request_id: request_id.map(str::to_string),
            recipient: recipient.to_string(),
            amount_sat,
        })? {
            Response::Proceed { request_id } => Ok(Begun::Proceed { request_id }),
            Response::Outcome(outcome) => Ok(Begun::Done(outcome)),
            Response::Error { code, message } => {
                Ok(Begun::Done(SendOutcome::op_error(&code, message)))
            }
            other => bail!("unexpected reply from satsd: {}", label(&other)),
        }
    }

    pub fn authorize(
        &mut self,
        token: &str,
        psbt: &str,
        excluded_utxos: u64,
    ) -> Result<SendOutcome> {
        self.outcome(Request::Authorize {
            protocol: PROTOCOL_VERSION,
            token: token.to_string(),
            psbt: psbt.to_string(),
            excluded_utxos,
        })
    }

    pub fn finish(&mut self, token: &str, outcome: BroadcastOutcome) -> Result<SendOutcome> {
        self.outcome(Request::Finish {
            protocol: PROTOCOL_VERSION,
            token: token.to_string(),
            outcome,
        })
    }

    fn outcome(&mut self, request: Request) -> Result<SendOutcome> {
        match self.call(request)? {
            Response::Outcome(outcome) => Ok(outcome),
            Response::Error { code, message } => Ok(SendOutcome::op_error(&code, message)),
            other => bail!("unexpected reply from satsd: {}", label(&other)),
        }
    }
}

fn label(response: &Response) -> &'static str {
    match response {
        Response::Status(_) => "status",
        Response::Unlock(_) => "unlock",
        Response::Ok => "ok",
        Response::Proceed { .. } => "proceed",
        Response::Outcome(_) => "outcome",
        Response::Error { .. } => "error",
    }
}
