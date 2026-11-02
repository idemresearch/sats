//! MCP Streamable HTTP for `sats agent serve --http`.
//!
//! The same keyless tool surface as stdio, for remote clients such as a
//! chat app connecting from its provider's servers. The server binds a
//! loopback address only; reaching it from the internet is the job of an
//! HTTPS tunnel or reverse proxy the human runs in front of it, so sats
//! never serves plaintext beyond this machine.
//!
//! The endpoint path carries the grant's bearer token, `/mcp/<token>`,
//! because remote chat clients offer OAuth or no authentication and no
//! custom header. Any other path is a 404 that names nothing. The gate
//! compares against the token's hash in constant time; every tool call
//! still re-reads the grant, so revocation, re-issue, and expiry cut off
//! a connected client at its next call, as on stdio.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use sats_core::token;

use super::server::SatsMcp;

/// The endpoint's path prefix; the token is the single segment after it.
const PREFIX: &str = "/mcp/";

/// `--http` and its companions, as parsed.
pub struct HttpOptions {
    /// Where to listen. Must be a loopback address.
    pub addr: SocketAddr,
    /// Host names a tunnel forwards with (the `Host` header it sends), in
    /// addition to loopback.
    pub public_hosts: Vec<String>,
}

impl HttpOptions {
    /// Refuse an address that would serve plaintext beyond this machine.
    pub fn check(&self) -> Result<()> {
        if !self.addr.ip().is_loopback() {
            bail!(
                "--http {} is not a loopback address — sats serves HTTP on loopback only; \
                 expose it with an HTTPS tunnel or reverse proxy, fx --http 127.0.0.1:{}",
                self.addr,
                self.addr.port()
            );
        }
        for host in &self.public_hosts {
            if host.is_empty() || host.contains(['/', '@', ' ']) {
                bail!("--public-host {host:?} is not a host name — pass fx sats.example.com");
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

/// Whether `path` is the token-bearing endpoint for `token_hash`.
fn authorized_path(path: &str, token_hash: &str) -> bool {
    match path.strip_prefix(PREFIX) {
        Some(candidate) if !candidate.is_empty() && !candidate.contains('/') => {
            token::verify(candidate, token_hash)
        }
        _ => false,
    }
}

fn not_found() -> Response<http_body_util::combinators::BoxBody<Bytes, std::convert::Infallible>> {
    let mut response = Response::new(Full::new(Bytes::new()).boxed());
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

pub async fn serve(service: SatsMcp, options: HttpOptions) -> Result<()> {
    let token_hash = Arc::new(
        token::hash(service.token()).context("cannot hash the agent token for the HTTP gate")?,
    );
    let config = StreamableHttpServerConfig::default().with_allowed_hosts(options.allowed_hosts());
    let mcp = StreamableHttpService::new(
        move || Ok(service.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );

    let listener = tokio::net::TcpListener::bind(options.addr)
        .await
        .with_context(|| format!("cannot listen on {}", options.addr))?;
    let bound = listener
        .local_addr()
        .context("cannot read the bound address")?;
    // The token stays out of the log: the human already holds it.
    eprintln!("sats agent serve: MCP Streamable HTTP on http://{bound}{PREFIX}<SATS_AGENT_TOKEN>");
    for host in &options.public_hosts {
        eprintln!(
            "sats agent serve: accepting https://{host}{PREFIX}<SATS_AGENT_TOKEN> via your tunnel"
        );
    }

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                eprintln!("sats agent serve: accept failed: {error}");
                continue;
            }
        };
        let (mcp, token_hash) = (mcp.clone(), token_hash.clone());
        tokio::spawn(async move {
            let handler = service_fn(move |request: Request<Incoming>| {
                let (mcp, token_hash) = (mcp.clone(), token_hash.clone());
                async move {
                    if !authorized_path(request.uri().path(), &token_hash) {
                        return Ok::<_, std::convert::Infallible>(not_found());
                    }
                    Ok(mcp.handle(request).await)
                }
            });
            // A dropped or malformed connection concerns only that client.
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), handler)
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(addr: &str, hosts: &[&str]) -> HttpOptions {
        HttpOptions {
            addr: addr.parse().unwrap(),
            public_hosts: hosts.iter().map(|h| h.to_string()).collect(),
        }
    }

    #[test]
    fn only_loopback_addresses_are_served() {
        assert!(options("127.0.0.1:8787", &[]).check().is_ok());
        assert!(options("[::1]:8787", &[]).check().is_ok());
        let error = options("0.0.0.0:8787", &[])
            .check()
            .unwrap_err()
            .to_string();
        assert!(error.contains("loopback"), "{error}");
        assert!(options("192.168.1.5:8787", &[]).check().is_err());
    }

    #[test]
    fn public_hosts_are_bare_host_names() {
        assert!(
            options("127.0.0.1:1", &["sats.example.com"])
                .check()
                .is_ok()
        );
        assert!(
            options("127.0.0.1:1", &["https://sats.example.com"])
                .check()
                .is_err()
        );
        assert!(
            options("127.0.0.1:1", &["user@sats.example.com"])
                .check()
                .is_err()
        );
        assert!(options("127.0.0.1:1", &[""]).check().is_err());
        assert_eq!(
            options("127.0.0.1:1", &["a.example"]).allowed_hosts(),
            ["localhost", "127.0.0.1", "::1", "a.example"]
        );
    }

    #[test]
    fn the_gate_admits_only_the_exact_token_path() {
        let minted = token::generate().unwrap();
        let raw: &str = minted.secret.as_str();
        let hash = minted.token_hash;
        assert!(authorized_path(&format!("/mcp/{raw}"), &hash));
        for path in [
            "/mcp".to_string(),
            "/mcp/".to_string(),
            "/".to_string(),
            format!("/mcp/{raw}/"),
            format!("/mcp/{raw}/x"),
            format!("/mcp/{raw}x"),
            format!("/x/mcp/{raw}"),
            "/mcp/sats_wrong".to_string(),
        ] {
            assert!(!authorized_path(&path, &hash), "{path} must be refused");
        }
    }
}
