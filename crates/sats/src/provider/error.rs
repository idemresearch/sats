//! Provider error taxonomy. Every variant that carries a URL carries the
//! *redacted* display form — secrets (subfrost path keys, bearer tokens)
//! must never reach an error string or a log line.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error(
        "no {cap} provider configured for {network} — add one under [providers] in config.toml"
    )]
    NoProvider {
        cap: &'static str,
        network: &'static str,
    },

    #[error(
        "multiple {cap} providers for {network} ({names}) — restrict one with capabilities = [...] or remove it"
    )]
    Ambiguous {
        cap: &'static str,
        network: &'static str,
        names: String,
    },

    #[error("invalid provider {name:?}: {reason}")]
    BadConfig { name: String, reason: String },

    #[error("provider {name:?} ({url}) serves a different network than {expected}")]
    WrongNetwork {
        name: String,
        url: String,
        expected: &'static str,
    },

    #[error("chain sync failed ({url}): {message}")]
    Sync { url: String, message: String },

    #[error("cannot estimate fee ({url}): {message}")]
    Fees { url: String, message: String },

    #[error("broadcast failed ({url}): {message}")]
    Broadcast { url: String, message: String },

    #[error(
        "guard {name} unreachable ({url}): {message} — refusing to plan without the asset check; retry, or pass --no-guards to plan anyway"
    )]
    Guard {
        name: String,
        url: String,
        message: String,
    },

    #[error("alkanes view failed ({url}): {message}")]
    View { url: String, message: String },
}

/// Origin-only form of a URL: `scheme://host[:port]`. Used for any endpoint
/// that may embed credentials in its path (subfrost API keys).
pub fn redact_url(url: &str) -> String {
    match url.find("://") {
        Some(scheme_end) => {
            let rest = &url[scheme_end + 3..];
            match rest.find('/') {
                Some(path_start) => url[..scheme_end + 3 + path_start].to_string(),
                None => url.to_string(),
            }
        }
        None => "<invalid-url>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_strips_path() {
        assert_eq!(
            redact_url("https://mainnet.subfrost.io/v4/SECRETKEY/jsonrpc"),
            "https://mainnet.subfrost.io"
        );
        assert_eq!(
            redact_url("http://localhost:3002/api"),
            "http://localhost:3002"
        );
        assert_eq!(redact_url("https://host.tld"), "https://host.tld");
        assert_eq!(redact_url("not a url"), "<invalid-url>");
    }
}
