//! sats human's side of a relay: link the device, mint connection codes, and
//! sync the mailbox.
//!
//! A relayed filing is decided here exactly as a local one: it goes
//! through [`crate::request::create`] (or `create_wrap`) with the agent's
//! token, so the grant ladder, idempotency, request ids, and the audit log
//! are the same code. The relay only transports; nothing it says is
//! trusted beyond being a proposal to check. What the device posts back is
//! the view a local `check_request` would return, rendered by the surface.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sats_core::authz::Grant;
use sats_core::bitcoin::Network;
use sats_core::relay::{
    self, AUTH_HEADER, CAPABILITY_VERSION, Capability, FILING_VERSION, HumanKey, RelayedFiling,
    RelayedOperation,
};
use sats_core::request::keyed_request_id;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::config::network_name;
use crate::request::{self, CreateError, CreateParams, CreateWrapParams, Credential, RelayGrant};
use crate::store::{Store, now_checked, write_atomic};

const TIMEOUT_SECS: u64 = 30;

/// This device's link to one relay, per network.
pub struct RelayLink {
    /// The relay's origin, fx `https://relay.example.com`.
    pub url: String,
    key: HumanKey,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkFile {
    url: String,
    /// The device's relay key. A communication credential: it reads this
    /// device's mailbox and answers for it, and opens nothing else.
    key: String,
}

impl Drop for LinkFile {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.key);
    }
}

fn link_path(store: &Store, network: Network) -> PathBuf {
    store
        .grants_dir(network_name(network))
        .with_file_name("relay.json")
}

/// Normalize a relay origin. HTTPS, or plain HTTP to loopback for local
/// testing; no path, query, or credentials, because device calls sign
/// the exact path the relay sees.
pub fn normalize_url(url: &str) -> Result<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let (scheme, rest) = trimmed.split_once("://").with_context(|| {
        format!("relay URL {url:?} needs a scheme, fx https://relay.example.com")
    })?;
    if rest.is_empty() || rest.contains(['/', '?', '#', '@']) {
        bail!("relay URL {url:?} must be an origin only, fx https://relay.example.com");
    }
    let host = rest
        .strip_prefix('[')
        .and_then(|h| h.split_once(']').map(|(h, _)| h))
        .unwrap_or_else(|| rest.rsplit_once(':').map_or(rest, |(h, _)| h));
    let loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    match scheme {
        "https" => {}
        "http" if loopback => {}
        "http" => bail!("relay URL {url:?} must use https (plain http is for loopback only)"),
        _ => bail!("relay URL {url:?} must use https"),
    }
    Ok(format!("{scheme}://{rest}"))
}

/// Link this device to a relay. An existing device key is kept unless
/// `new_key`: a new key retires every connection code minted with the old one.
pub fn link(store: &Store, network: Network, url: &str, new_key: bool) -> Result<RelayLink> {
    sats_core::network::ensure_test_network(network)?;
    let url = normalize_url(url)?;
    let key = match load(store, network)? {
        Some(existing) if !new_key => existing.key,
        _ => HumanKey::generate()?,
    };
    let file = LinkFile {
        url: url.clone(),
        key: key.to_hex().to_string(),
    };
    let path = link_path(store, network);
    store.create_private_dirs(path.parent().context("relay path has no parent")?)?;
    write_atomic(&path, &serde_json::to_vec_pretty(&file)?, true)?;
    Ok(RelayLink { url, key })
}

pub fn load(store: &Store, network: Network) -> Result<Option<RelayLink>> {
    let path = link_path(store, network);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => Zeroizing::new(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let file: LinkFile = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "unreadable relay link {} — run sats relay link <URL> again",
            path.display()
        )
    })?;
    Ok(Some(RelayLink {
        url: normalize_url(&file.url)?,
        key: HumanKey::from_hex(&file.key)?,
    }))
}

impl RelayLink {
    /// The mailbox name: this device's relay public key.
    pub fn human(&self) -> String {
        self.key.public_hex()
    }

    /// The relay's MCP endpoint for agents that sign in with OAuth.
    pub fn mcp_url(&self) -> String {
        format!("{}/mcp", self.url)
    }

    /// The connection code for `grant`: the device-signed capability an
    /// agent's human pastes on the relay's OAuth sign-in page. It carries
    /// no grant token, but it names the grant instance and is the agent's
    /// credential at the relay: shown once, never put in a URL.
    pub fn connection_code(&self, grant: &Grant) -> Result<Zeroizing<String>> {
        let capability = Capability {
            v: CAPABILITY_VERSION,
            network: grant.network.clone(),
            human: self.human(),
            agent: grant.agent.clone(),
            grant_id: grant.grant_id.clone(),
            expires_at: grant.expires_at,
        };
        Ok(Zeroizing::new(capability.encode(&self.key)?))
    }

    fn call(&self, method: &str, path: &str, body: &[u8]) -> Result<serde_json::Value> {
        let header = relay::sign_call(&self.key, method, path, now_checked()?, body);
        let url = format!("{}{path}", self.url);
        let request = match method {
            "GET" => minreq::get(&url),
            _ => minreq::post(&url)
                .with_header("Content-Type", "application/json")
                .with_body(body.to_vec()),
        };
        let response = request
            .with_header(AUTH_HEADER, header)
            .with_timeout(TIMEOUT_SECS)
            .send()
            .with_context(|| format!("cannot reach the relay at {}", self.url))?;
        let parsed: serde_json::Value =
            serde_json::from_slice(response.as_bytes()).with_context(|| {
                format!("the relay answered {} with non-JSON", response.status_code)
            })?;
        if response.status_code != 200 {
            bail!(
                "the relay refused {method} {path} ({}): {}",
                response.status_code,
                parsed["message"].as_str().unwrap_or("no message")
            );
        }
        Ok(parsed)
    }
}

/// What became of one relayed filing, for the surface to render as the
/// agent's view.
pub enum Outcome {
    /// A request exists on this device under that id.
    Record { agent: String, request_id: String },
    /// The device refused the filing outright (a typed operational code,
    /// fx `no_grant` after a revoke). Final for this filing. `grant` names
    /// the agent and grant instance when the filing's connection code was
    /// this device's own; only then can the view be sealed for the agent.
    Refused {
        request_id: String,
        code: String,
        message: String,
        grant: Option<(String, String)>,
    },
}

impl Outcome {
    fn request_id(&self) -> &str {
        match self {
            Outcome::Record { request_id, .. } | Outcome::Refused { request_id, .. } => request_id,
        }
    }
}

/// One sync's results.
#[derive(Default)]
pub struct SyncReport {
    /// Requests filed on this device by this sync.
    pub created: Vec<String>,
    /// Filings already known here whose state was re-posted.
    pub refreshed: Vec<String>,
    /// Filings refused outright: (request id, code).
    pub refused: Vec<(String, String)>,
    /// Filings left for a later sync after a transient failure: (id, why).
    pub deferred: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct Inbox {
    filings: Vec<InboxFiling>,
}

#[derive(Deserialize)]
struct InboxFiling {
    request_id: String,
    sealed: String,
}

/// Codes worth retrying: the filing wasn't decided, only interrupted.
fn transient(error: &CreateError) -> bool {
    matches!(
        error,
        CreateError::Provider(_)
            | CreateError::Alkanes(_)
            | CreateError::Store(_)
            | CreateError::ClockUnavailable(_)
    )
}

fn decide(
    store: &Store,
    network: Network,
    link: &RelayLink,
    item: &InboxFiling,
    resolve_services: &dyn Fn() -> Result<crate::provider::Services>,
    report: &mut SyncReport,
) -> Option<Outcome> {
    let refuse = |code: &str, message: &str, grant: Option<(String, String)>| Outcome::Refused {
        request_id: item.request_id.clone(),
        code: code.into(),
        message: message.into(),
        grant,
    };
    let filing = STANDARD
        .decode(&item.sealed)
        .ok()
        .and_then(|sealed| {
            relay::open(
                &link.key,
                &relay::envelope_aad(&link.human(), &item.request_id),
                &sealed,
            )
            .ok()
        })
        .and_then(|plain| serde_json::from_slice::<RelayedFiling>(&plain).ok());
    let Some(filing) = filing else {
        return Some(refuse(
            "unreadable_filing",
            "the device could not open this request",
            None,
        ));
    };
    // The relay is untrusted: accept only a live connection code this
    // device signed itself, and take the agent and grant from it.
    let now = match now_checked() {
        Ok(now) => now,
        Err(error) => {
            report
                .deferred
                .push((item.request_id.clone(), format!("{error:#}")));
            return None;
        }
    };
    let capability = match Capability::verify_own(&filing.capability, &link.key, now) {
        Ok(capability) if capability.network == network_name(network) => capability,
        _ => {
            return Some(refuse(
                "unknown_connection",
                "the request came through a connection code this device didn't issue, or one \
                 that has expired",
                None,
            ));
        }
    };
    let grant = Some((capability.agent.clone(), capability.grant_id.clone()));
    if filing.v != FILING_VERSION
        || filing.request_id != item.request_id
        || keyed_request_id(
            &capability.grant_id,
            &capability.agent,
            &filing.idempotency_key,
        ) != filing.request_id
    {
        return Some(refuse(
            "malformed_filing",
            "the relayed request is inconsistent",
            grant,
        ));
    }
    let agent = capability.agent.clone();
    let net_name = network_name(network);
    if let Ok(Some(_)) = store.load_agent_request(net_name, &agent, &filing.request_id) {
        report.refreshed.push(filing.request_id.clone());
        return Some(Outcome::Record {
            agent,
            request_id: filing.request_id,
        });
    }
    let credential = Credential::RelayGrant(RelayGrant::verified(&capability.grant_id));
    let created = match &filing.operation {
        RelayedOperation::Send {
            address,
            amount_sat,
        } => request::create(
            store,
            network,
            &CreateParams {
                agent: &agent,
                credential,
                idempotency_key: &filing.idempotency_key,
                address,
                amount_sat: *amount_sat,
            },
        ),
        RelayedOperation::FrbtcWrap { amount_sat } => request::create_wrap(
            store,
            network,
            &CreateWrapParams {
                agent: &agent,
                credential,
                idempotency_key: &filing.idempotency_key,
                amount_sat: *amount_sat,
            },
            resolve_services,
        ),
    };
    match created {
        Ok(record) if record.id == filing.request_id => {
            report.created.push(record.id.clone());
            Some(Outcome::Record {
                agent,
                request_id: record.id,
            })
        }
        // The relay's id derivation and this device's disagree: the
        // filing names a grant instance other than the one on file.
        Ok(_) => Some(refuse(
            "grant_mismatch",
            "the request names a different grant",
            grant,
        )),
        Err(error) if transient(&error) => {
            report
                .deferred
                .push((filing.request_id, error.message(&agent)));
            None
        }
        Err(error) => Some(refuse(error.code(), &error.message(&agent), grant)),
    }
}

/// One posted result: the view sealed under its grant's result key, so
/// the relay reads it only while serving that grant's connection. The
/// status stays readable: the relay uses it to clear finished filings.
fn result_entry(
    store: &Store,
    network: Network,
    link: &RelayLink,
    outcome: &Outcome,
    render: &dyn Fn(&Outcome) -> serde_json::Value,
) -> Result<serde_json::Value> {
    let view = render(outcome);
    let status = view["status"].as_str().unwrap_or("error").to_string();
    let net_name = network_name(network);
    let grant = match outcome {
        Outcome::Record { agent, request_id } => store
            .load_agent_request(net_name, agent, request_id)?
            .map(|record| (agent.clone(), record.grant_id)),
        Outcome::Refused { grant, .. } => grant.clone(),
    };
    let sealed = match grant {
        Some((agent, grant_id)) => {
            let key = relay::result_key(net_name, &link.human(), &agent, &grant_id);
            let bytes = serde_json::to_vec(&view)?;
            Some(STANDARD.encode(relay::seal_result(&key, outcome.request_id(), &bytes)?))
        }
        None => None,
    };
    Ok(serde_json::json!({
        "request_id": outcome.request_id(),
        "revision": revision(),
        "status": status,
        "sealed": sealed,
    }))
}

/// Fetch this device's mailbox, decide each filing here, and post the
/// agent's view of each back. `render` turns an outcome into that view.
pub fn sync(
    store: &Store,
    network: Network,
    link: &RelayLink,
    resolve_services: &dyn Fn() -> Result<crate::provider::Services>,
    render: &dyn Fn(&Outcome) -> serde_json::Value,
) -> Result<SyncReport> {
    sats_core::network::ensure_test_network(network)?;
    let inbox: Inbox = serde_json::from_value(link.call("GET", "/v1/inbox", b"")?)
        .context("the relay's inbox is malformed")?;
    let mut report = SyncReport::default();
    let mut results = Vec::new();
    for item in &inbox.filings {
        let Some(outcome) = decide(store, network, link, item, resolve_services, &mut report)
        else {
            continue;
        };
        if let Outcome::Refused {
            request_id, code, ..
        } = &outcome
        {
            report.refused.push((request_id.clone(), code.clone()));
        }
        results.push(result_entry(store, network, link, &outcome, render)?);
    }
    if !results.is_empty() {
        let body = serde_json::to_vec(&serde_json::json!({"results": results}))?;
        let answer = link.call("POST", "/v1/results", &body)?;
        if let Some(rejected) = answer["rejected"].as_array().filter(|r| !r.is_empty()) {
            log::warn!("the relay rejected some results: {rejected:?}");
        }
    }
    Ok(report)
}

/// Post the current view of requests this device already decided, fx
/// right after the human approves or dismisses one, so the agent sees it
/// without waiting for a sync. The relay ignores ids it never carried,
/// so posting a local-only request is harmless. Returns how many the
/// relay accepted.
pub fn post_outcomes(
    store: &Store,
    network: Network,
    link: &RelayLink,
    outcomes: &[Outcome],
    render: &dyn Fn(&Outcome) -> serde_json::Value,
) -> Result<usize> {
    if outcomes.is_empty() {
        return Ok(0);
    }
    let results = outcomes
        .iter()
        .map(|outcome| result_entry(store, network, link, outcome, render))
        .collect::<Result<Vec<_>>>()?;
    let body = serde_json::to_vec(&serde_json::json!({"results": results}))?;
    let answer = link.call("POST", "/v1/results", &body)?;
    Ok(answer["accepted"].as_array().map_or(0, Vec::len))
}

/// A strictly increasing revision for this device's results.
fn revision() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0);
    let mut last = LAST.load(Ordering::Relaxed);
    loop {
        let next = now.max(last + 1);
        match LAST.compare_exchange(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(seen) => last = seen,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_urls_are_https_origins_or_loopback_http() {
        assert_eq!(
            normalize_url("https://relay.example.com/").unwrap(),
            "https://relay.example.com"
        );
        assert!(normalize_url("http://127.0.0.1:8080").is_ok());
        assert!(normalize_url("http://localhost:8080").is_ok());
        assert!(normalize_url("http://[::1]:8080").is_ok());
        for bad in [
            "relay.example.com",
            "http://relay.example.com",
            "ftp://relay.example.com",
            "https://relay.example.com/prefix",
            "https://user@relay.example.com",
            "https://relay.example.com?x=1",
            "https://",
        ] {
            assert!(normalize_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn linking_keeps_the_device_key_and_stores_it_privately() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path());
        let first = link(&store, Network::Signet, "https://a.example", false).unwrap();
        let again = link(&store, Network::Signet, "https://b.example", false).unwrap();
        assert_eq!(first.human(), again.human());
        let rotated = link(&store, Network::Signet, "https://b.example", true).unwrap();
        assert_ne!(rotated.human(), first.human());
        assert_eq!(
            load(&store, Network::Signet).unwrap().unwrap().human(),
            rotated.human()
        );
        let loaded = load(&store, Network::Signet).unwrap().unwrap();
        assert_eq!(loaded.url, "https://b.example");
        assert!(load(&store, Network::Regtest).unwrap().is_none());
        assert!(link(&store, Network::Bitcoin, "https://a.example", false).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(link_path(&store, Network::Signet))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn revisions_strictly_increase() {
        let a = revision();
        let b = revision();
        assert!(b > a);
    }
}
