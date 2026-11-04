//! OAuth 2.1 for agents that sign in to the relay (the MCP authorization
//! flow ChatGPT and similar clients use).
//!
//! The relay is the authorization server and the protected resource. A
//! client registers itself (dynamic client registration, public clients
//! with PKCE), sends the human to `/oauth/authorize`, and the human pastes
//! the connection code `sats agent grant --relay` printed: the device-signed
//! capability. That paste is the consent. The client then trades the code
//! for an access token (one hour) and a refresh token (rotated on every
//! use), neither outliving the capability.
//!
//! The capability stays the only credential: a token resolves to it, and
//! every MCP call re-verifies its signature and expiry. The device still
//! decides every filing against its own grant.
//!
//! Stored tokens are unreadable at rest: each record is sealed under a key
//! derived from the token itself and filed under a second derivation, so
//! the relay's database alone yields neither tokens nor capabilities.

use std::sync::Mutex;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use rusqlite::{Connection, OptionalExtension, params};
use sats_core::bitcoin::hashes::{Hash, HashEngine, sha256};
use serde::{Deserialize, Serialize};

/// Seconds an authorization code lives.
pub const CODE_TTL: u64 = 300;
/// Seconds an access token lives, at most.
pub const ACCESS_TTL: u64 = 3600;
/// Seconds a refresh token lives, at most.
pub const REFRESH_TTL: u64 = 30 * 24 * 3600;
/// Redirect URIs one client may register.
const MAX_REDIRECTS: usize = 10;

#[derive(Clone, Copy)]
pub enum Kind {
    Code,
    Access,
    Refresh,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Code => "code",
            Kind::Access => "access",
            Kind::Refresh => "refresh",
        }
    }
}

/// What a code or token stands for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The encoded, device-signed capability.
    pub capability: String,
    pub client_id: String,
    /// For codes: the redirect URI and PKCE challenge they were issued for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_challenge: Option<String>,
}

fn derive(label: &str, token: &str) -> [u8; 32] {
    let mut engine = sha256::Hash::engine();
    engine.input(b"sats-relay/oauth/v1\0");
    engine.input(label.as_bytes());
    engine.input(&[0]);
    engine.input(token.as_bytes());
    sha256::Hash::from_engine(engine).to_byte_array()
}

fn random(bytes: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; bytes];
    getrandom::fill(&mut out).map_err(|_| anyhow::anyhow!("the OS random source failed"))?;
    Ok(out)
}

/// `BASE64URL(SHA256(verifier))`, the S256 PKCE challenge.
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(sha256::Hash::hash(verifier.as_bytes()).to_byte_array())
}

/// A verifier is 43-128 characters of the RFC 7636 alphabet.
pub fn valid_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
}

/// Redirect URIs a client may register: HTTPS, or HTTP to loopback for
/// local clients. No fragments.
pub fn valid_redirect(uri: &str) -> bool {
    if uri.len() > 2000 || uri.contains('#') {
        return false;
    }
    let Some((scheme, rest)) = uri.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    let host = authority
        .strip_prefix('[')
        .and_then(|h| h.split_once(']').map(|(h, _)| h))
        .unwrap_or_else(|| authority.rsplit_once(':').map_or(authority, |(h, _)| h));
    if host.is_empty() || authority.contains('@') {
        return false;
    }
    match scheme {
        "https" => true,
        "http" => {
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }
        _ => false,
    }
}

pub struct OAuth {
    conn: Mutex<Connection>,
}

impl OAuth {
    pub fn open(path: &std::path::Path) -> Result<OAuth> {
        let conn =
            Connection::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS oauth_clients (
                 client_id TEXT PRIMARY KEY,
                 redirect_uris TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS oauth_tokens (
                 id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 sealed BLOB NOT NULL,
                 expires_at INTEGER NOT NULL
             );",
        )
        .context("cannot prepare the OAuth tables")?;
        Ok(OAuth {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Register a public client; returns its id.
    pub fn register(&self, redirect_uris: &[String], now: u64) -> Result<Option<String>> {
        if redirect_uris.is_empty()
            || redirect_uris.len() > MAX_REDIRECTS
            || !redirect_uris.iter().all(|uri| valid_redirect(uri))
        {
            return Ok(None);
        }
        let client_id = format!("c-{}", hex_lower(&random(16)?));
        self.conn().execute(
            "INSERT INTO oauth_clients (client_id, redirect_uris, created_at) VALUES (?1, ?2, ?3)",
            params![client_id, serde_json::to_string(redirect_uris)?, now as i64],
        )?;
        Ok(Some(client_id))
    }

    /// Whether `redirect_uri` is exactly one `client_id` registered.
    pub fn redirect_allowed(&self, client_id: &str, redirect_uri: &str) -> Result<bool> {
        let stored: Option<String> = self
            .conn()
            .query_row(
                "SELECT redirect_uris FROM oauth_clients WHERE client_id = ?1",
                params![client_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(stored
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .is_some_and(|uris| uris.iter().any(|uri| uri == redirect_uri)))
    }

    /// Issue a code or token for `record`, valid for `ttl` seconds.
    pub fn issue(&self, kind: Kind, record: &Record, ttl: u64, now: u64) -> Result<String> {
        let token = URL_SAFE_NO_PAD.encode(random(32)?);
        let nonce = random(12)?;
        let sealed = ChaCha20Poly1305::new((&derive("key", &token)).into())
            .encrypt(
                Nonce::from_slice(&nonce),
                serde_json::to_vec(record)?.as_slice(),
            )
            .map_err(|_| anyhow::anyhow!("cannot seal the token record"))?;
        let mut blob = nonce;
        blob.extend_from_slice(&sealed);
        let conn = self.conn();
        conn.execute(
            "DELETE FROM oauth_tokens WHERE expires_at <= ?1",
            params![now as i64],
        )?;
        conn.execute(
            "INSERT INTO oauth_tokens (id, kind, sealed, expires_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                hex_lower(&derive("id", &token)),
                kind.as_str(),
                blob,
                now.saturating_add(ttl) as i64
            ],
        )?;
        Ok(token)
    }

    /// The record behind a live `token` of `kind`. With `consume`, the
    /// token is spent: codes and refresh tokens work exactly once.
    pub fn redeem(
        &self,
        kind: Kind,
        token: &str,
        now: u64,
        consume: bool,
    ) -> Result<Option<Record>> {
        if token.is_empty() || token.len() > 256 {
            return Ok(None);
        }
        let id = hex_lower(&derive("id", token));
        let conn = self.conn();
        let row: Option<(String, Vec<u8>, i64)> = conn
            .query_row(
                "SELECT kind, sealed, expires_at FROM oauth_tokens WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((stored_kind, blob, expires_at)) = row else {
            return Ok(None);
        };
        if consume {
            conn.execute("DELETE FROM oauth_tokens WHERE id = ?1", params![id])?;
        }
        if stored_kind != kind.as_str() || (expires_at as u64) <= now || blob.len() < 12 {
            return Ok(None);
        }
        let plain = ChaCha20Poly1305::new((&derive("key", token)).into())
            .decrypt(Nonce::from_slice(&blob[..12]), &blob[12..])
            .ok();
        Ok(plain.and_then(|p| serde_json::from_slice(&p).ok()))
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode `application/x-www-form-urlencoded` (also a query string).
pub fn parse_form(input: &str) -> Vec<(String, String)> {
    fn decode(part: &str) -> String {
        let bytes = part.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'+' => out.push(b' '),
                b'%' if i + 2 < bytes.len() => {
                    let hex = |b: u8| (b as char).to_digit(16);
                    match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                        (Some(hi), Some(lo)) => {
                            out.push((hi * 16 + lo) as u8);
                            i += 2;
                        }
                        _ => out.push(b'%'),
                    }
                }
                byte => out.push(byte),
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    input
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect()
}

/// Encode one value for a query string.
pub fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Escape text for HTML element content and attribute values.
pub fn html_escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\'' => "&#39;".into(),
            c => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, OAuth) {
        let dir = tempfile::TempDir::new().unwrap();
        let oauth = OAuth::open(&dir.path().join("relay.db")).unwrap();
        (dir, oauth)
    }

    fn record() -> Record {
        Record {
            capability: "payload.signature".into(),
            client_id: "c-1".into(),
            redirect_uri: None,
            code_challenge: None,
        }
    }

    #[test]
    fn tokens_redeem_by_kind_once_when_consumed_and_expire() {
        let (dir, oauth) = store();
        let code = oauth.issue(Kind::Code, &record(), 60, 1000).unwrap();
        assert!(
            oauth
                .redeem(Kind::Access, &code, 1000, false)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            oauth.redeem(Kind::Code, &code, 1000, true).unwrap(),
            Some(record())
        );
        assert!(
            oauth
                .redeem(Kind::Code, &code, 1000, true)
                .unwrap()
                .is_none()
        );

        let access = oauth.issue(Kind::Access, &record(), 60, 1000).unwrap();
        assert!(
            oauth
                .redeem(Kind::Access, &access, 1059, false)
                .unwrap()
                .is_some()
        );
        assert!(
            oauth
                .redeem(Kind::Access, &access, 1059, false)
                .unwrap()
                .is_some()
        );
        assert!(
            oauth
                .redeem(Kind::Access, &access, 1060, false)
                .unwrap()
                .is_none()
        );
        assert!(
            oauth
                .redeem(Kind::Access, "forged", 1000, false)
                .unwrap()
                .is_none()
        );

        // At rest: neither the token nor the capability is readable.
        drop(oauth);
        let db = std::fs::read(dir.path().join("relay.db")).unwrap();
        for secret in [access.as_bytes(), b"payload.signature".as_slice()] {
            assert!(!db.windows(secret.len()).any(|w| w == secret));
        }
    }

    #[test]
    fn clients_register_only_safe_redirects_and_match_exactly() {
        let (_dir, oauth) = store();
        let uris = vec!["https://chatgpt.com/connector_platform_oauth_redirect".to_string()];
        let id = oauth.register(&uris, 1).unwrap().unwrap();
        assert!(oauth.redirect_allowed(&id, &uris[0]).unwrap());
        assert!(
            !oauth
                .redirect_allowed(&id, "https://chatgpt.com/other")
                .unwrap()
        );
        assert!(!oauth.redirect_allowed("c-unknown", &uris[0]).unwrap());
        for bad in [
            vec![],
            vec!["http://evil.example/cb".to_string()],
            vec!["javascript:alert(1)".to_string()],
            vec!["https://a.example/cb#frag".to_string()],
            vec!["https://user@a.example/cb".to_string()],
        ] {
            assert!(oauth.register(&bad, 1).unwrap().is_none(), "{bad:?}");
        }
        assert!(valid_redirect("http://127.0.0.1:3000/cb"));
        assert!(valid_redirect("http://localhost/cb"));
    }

    #[test]
    fn pkce_matches_the_rfc_7636_example() {
        // RFC 7636, Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(valid_verifier(verifier));
        assert!(!valid_verifier("short"));
        assert!(!valid_verifier(&"a".repeat(129)));
        assert!(!valid_verifier(&format!("{}!", "a".repeat(43))));
    }

    #[test]
    fn forms_decode_and_values_encode() {
        assert_eq!(
            parse_form("a=1&b=x+y&c=%2Fz%3D&d&e=%zz"),
            vec![
                ("a".into(), "1".into()),
                ("b".into(), "x y".into()),
                ("c".into(), "/z=".into()),
                ("d".into(), "".into()),
                ("e".into(), "%zz".into()),
            ]
        );
        assert_eq!(encode("a b/c=d"), "a%20b%2Fc%3Dd");
        assert_eq!(
            parse_form(&format!("v={}", encode("ü & ?"))),
            vec![("v".into(), "ü & ?".into())]
        );
        assert_eq!(parse_form("x=%é1"), vec![("x".into(), "%é1".into())]);
        assert_eq!(
            html_escape("<a href=\"x\">'&"),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;"
        );
    }
}
