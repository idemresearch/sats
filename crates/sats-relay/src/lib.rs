//! sats relay: a keyless intermediary between sats agent and sats human.
//!
//! An agent that can't reach the human's device (a chat app on a phone,
//! connecting from its provider's servers) talks MCP to the relay. The
//! relay verifies the human-signed capability in the URL, seals each
//! filing to the human's device, and serves back the results the device
//! posts. It never holds a wallet, a seed, or a grant policy, and it can't
//! approve, sign, or broadcast: the device re-checks every filing against
//! its own grant and the human approves there.
//!
//! Routes:
//! - `/mcp/<capability>`: MCP Streamable HTTP for the agent.
//! - `GET /v1/inbox`, `POST /v1/results`: the device, each call signed
//!   with its relay key ([`sats_core::relay::AUTH_HEADER`]).
//! - `GET /healthz`: liveness.

pub mod mailbox;
mod tools;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use sats_core::relay::{self, Capability};
use serde::Deserialize;

use crate::mailbox::Mailbox;

/// The agent endpoint's prefix; the capability is the segment after it.
const MCP_PREFIX: &str = "/mcp/";
/// The largest device request body accepted.
const MAX_DEVICE_BODY: usize = 1 << 20;

pub struct Config {
    pub listen: SocketAddr,
    /// Host names clients use to reach the relay (the `Host` header).
    pub public_hosts: Vec<String>,
    pub data_dir: PathBuf,
    /// Seconds a filing waits for the device.
    pub filing_ttl: u64,
    /// Seconds a result stays observable.
    pub result_ttl: u64,
}

impl Config {
    /// Refuse a configuration that would answer every request with 403.
    pub fn check(&self) -> Result<()> {
        if !self.listen.ip().is_loopback() && self.public_hosts.is_empty() {
            bail!(
                "listening on {} needs the public host name clients use — pass --public-host \
                 (or SATS_RELAY_PUBLIC_HOST), fx relay.example.com",
                self.listen
            );
        }
        for host in &self.public_hosts {
            if host.is_empty() || host.contains(['/', '@', ' ']) {
                bail!("public host {host:?} is not a host name — pass fx relay.example.com");
            }
        }
        Ok(())
    }

    fn allowed_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = ["localhost", "127.0.0.1", "::1"].map(String::from).to_vec();
        hosts.extend(self.public_hosts.iter().cloned());
        hosts
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

type Body = BoxBody<Bytes, Infallible>;

fn respond(status: StatusCode, body: serde_json::Value) -> Response<Body> {
    let mut response = Response::new(Full::new(Bytes::from(body.to_string())).boxed());
    *response.status_mut() = status;
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    response
}

fn not_found() -> Response<Body> {
    let mut response = Response::new(Full::new(Bytes::new()).boxed());
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

fn refused(status: StatusCode, code: &str, message: &str) -> Response<Body> {
    respond(
        status,
        serde_json::json!({"error_code": code, "message": message}),
    )
}

struct Relay {
    mailbox: Arc<Mailbox>,
    mcp: StreamableHttpService<tools::RelayMcp, LocalSessionManager>,
    hosts: Vec<String>,
}

/// Serve on an already-bound listener until the process ends.
pub async fn serve(listener: tokio::net::TcpListener, config: Config) -> Result<()> {
    config.check()?;
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("cannot create {}", config.data_dir.display()))?;
    let mailbox = Arc::new(Mailbox::open(
        &config.data_dir.join("relay.db"),
        config.filing_ttl,
        config.result_ttl,
    )?);
    let tools_mailbox = mailbox.clone();
    let mcp = StreamableHttpService::new(
        move || Ok(tools::RelayMcp::new(tools_mailbox.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_allowed_hosts(config.allowed_hosts()),
    );
    let relay = Arc::new(Relay {
        mailbox,
        mcp,
        hosts: config.allowed_hosts(),
    });

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                eprintln!("sats-relay: accept failed: {error}");
                continue;
            }
        };
        let relay = relay.clone();
        tokio::spawn(async move {
            let handler = service_fn(move |request: Request<Incoming>| {
                let relay = relay.clone();
                async move { Ok::<_, Infallible>(route(&relay, request).await) }
            });
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), handler)
                .await;
        });
    }
}

fn host_allowed(relay: &Relay, request: &Request<Incoming>) -> bool {
    let Some(host) = request
        .headers()
        .get(hyper::header::HOST)
        .and_then(|h| h.to_str().ok())
    else {
        return false;
    };
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.split_once(']').map(|(h, _)| h))
        .unwrap_or_else(|| host.rsplit_once(':').map_or(host, |(h, _)| h));
    relay
        .hosts
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(host) || allowed.eq_ignore_ascii_case(bare))
}

async fn route(relay: &Relay, mut request: Request<Incoming>) -> Response<Body> {
    let path = request.uri().path().to_string();
    if let Some(encoded) = path.strip_prefix(MCP_PREFIX) {
        // An unverifiable capability gets the same empty 404 as any
        // unknown path: the relay names nothing to a stranger.
        let Ok(capability) = Capability::decode(encoded) else {
            return not_found();
        };
        request.extensions_mut().insert(capability);
        return relay.mcp.handle(request).await;
    }
    if !host_allowed(relay, &request) {
        return refused(
            StatusCode::FORBIDDEN,
            "forbidden_host",
            "unknown Host header",
        );
    }
    match (request.method().clone(), path.as_str()) {
        (Method::GET, "/healthz") => respond(StatusCode::OK, serde_json::json!({"ok": true})),
        (Method::GET, "/v1/inbox") => device(relay, request, inbox).await,
        (Method::POST, "/v1/results") => device(relay, request, results).await,
        _ => not_found(),
    }
}

/// Authenticate a device call, then answer it.
async fn device(
    relay: &Relay,
    request: Request<Incoming>,
    answer: fn(&Mailbox, &str, &[u8]) -> Response<Body>,
) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let Ok(body) = Limited::new(body, MAX_DEVICE_BODY).collect().await else {
        return refused(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "request body too large",
        );
    };
    let body = body.to_bytes();
    let Some(header) = parts
        .headers
        .get(relay::AUTH_HEADER)
        .and_then(|h| h.to_str().ok())
    else {
        return refused(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing device signature",
        );
    };
    match relay::verify_call(
        header,
        parts.method.as_str(),
        parts.uri.path(),
        &body,
        now(),
    ) {
        Ok(human) => answer(&relay.mailbox, &human, &body),
        Err(relay::RelayError::Stale) => refused(
            StatusCode::UNAUTHORIZED,
            "stale_call",
            "the device clock is more than 5 minutes from the relay's",
        ),
        Err(_) => refused(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "bad device signature",
        ),
    }
}

fn inbox(mailbox: &Mailbox, human: &str, _body: &[u8]) -> Response<Body> {
    match mailbox.inbox(human, now()) {
        Ok(filings) => respond(
            StatusCode::OK,
            serde_json::json!({
                "filings": filings
                    .into_iter()
                    .map(|(request_id, sealed)| serde_json::json!({
                        "request_id": request_id,
                        "sealed": STANDARD.encode(sealed),
                    }))
                    .collect::<Vec<_>>(),
            }),
        ),
        Err(error) => store_error(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PostedResults {
    results: Vec<PostedResult>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PostedResult {
    request_id: String,
    revision: u64,
    view: serde_json::Value,
}

fn results(mailbox: &Mailbox, human: &str, body: &[u8]) -> Response<Body> {
    let Ok(posted) = serde_json::from_slice::<PostedResults>(body) else {
        return refused(
            StatusCode::BAD_REQUEST,
            "malformed",
            "expected {\"results\": [...]}",
        );
    };
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    for result in posted.results {
        // The view is what check_request will return: it must at least
        // be an object naming this request and a status.
        let well_formed = result.view.is_object()
            && result.view.get("status").is_some_and(|s| s.is_string())
            && result
                .view
                .get("request_id")
                .is_none_or(|id| id.as_str() == Some(&result.request_id));
        if !well_formed {
            rejected
                .push(serde_json::json!({"request_id": result.request_id, "reason": "malformed"}));
            continue;
        }
        match mailbox.post_result(
            human,
            &result.request_id,
            result.revision,
            &result.view,
            now(),
        ) {
            Ok(Ok(())) => accepted.push(result.request_id),
            Ok(Err(why)) => rejected.push(serde_json::json!({
                "request_id": result.request_id,
                "reason": match why {
                    mailbox::Rejected::Unknown => "unknown",
                    mailbox::Rejected::Stale => "stale",
                },
            })),
            Err(error) => return store_error(error),
        }
    }
    respond(
        StatusCode::OK,
        serde_json::json!({"accepted": accepted, "rejected": rejected}),
    )
}

fn store_error(error: anyhow::Error) -> Response<Body> {
    eprintln!("sats-relay: store error: {error:#}");
    refused(
        StatusCode::INTERNAL_SERVER_ERROR,
        "store_error",
        "the relay's store failed",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(listen: &str, hosts: &[&str]) -> Config {
        Config {
            listen: listen.parse().unwrap(),
            public_hosts: hosts.iter().map(|h| h.to_string()).collect(),
            data_dir: PathBuf::from("/nonexistent"),
            filing_ttl: 1,
            result_ttl: 1,
        }
    }

    #[test]
    fn a_public_listener_needs_its_host_name() {
        assert!(config("127.0.0.1:8080", &[]).check().is_ok());
        let error = config("0.0.0.0:8080", &[]).check().unwrap_err().to_string();
        assert!(error.contains("--public-host"), "{error}");
        assert!(
            config("0.0.0.0:8080", &["relay.example.com"])
                .check()
                .is_ok()
        );
        assert!(
            config("0.0.0.0:8080", &["https://relay.example.com"])
                .check()
                .is_err()
        );
    }
}
