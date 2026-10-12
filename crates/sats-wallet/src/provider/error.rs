//! Provider error taxonomy. Every variant that carries a URL carries the
//! *redacted* display form — secrets (subfrost path keys, bearer tokens)
//! must never reach an error string or a log line.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProviderError {
    /// A configured choice that cannot be served: the reason names the fix.
    #[error("{network} providers: {reason}")]
    Setup {
        network: &'static str,
        reason: String,
    },

    #[cfg(feature = "experimental-alkanes")]
    #[error(
        "Alkanes on {network} needs Subfrost as the provider — run `sats providers add subfrost`"
    )]
    NoAlkanes { network: &'static str },

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

    #[cfg(feature = "experimental-alkanes")]
    #[error("alkanes view failed ({url}): {message}")]
    View { url: String, message: String },
}

/// Display-only origin: never expose user-info, paths, queries or fragments.
/// This intentionally accepts only an unambiguous HTTP(S) authority; it is
/// not a URL parser and never changes the URL used by the transport.
pub fn redact_url(url: &str) -> String {
    let invalid = || "<invalid-url>".to_string();
    let Some((scheme, rest)) = url.split_once("://") else {
        return invalid();
    };
    if !matches!(scheme, "http" | "https") {
        return invalid();
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let (host, port) = if let Some(ipv6) = host_port.strip_prefix('[') {
        let Some((address, suffix)) = ipv6.split_once(']') else {
            return invalid();
        };
        if address.parse::<std::net::Ipv6Addr>().is_err() {
            return invalid();
        }
        (&host_port[..address.len() + 2], suffix)
    } else {
        let (host, port) = host_port
            .find(':')
            .map(|index| host_port.split_at(index))
            .unwrap_or((host_port, ""));
        if host.is_empty()
            || !host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        {
            return invalid();
        }
        (host, port)
    };
    if !port.is_empty()
        && !port.strip_prefix(':').is_some_and(|value| {
            !value.is_empty()
                && value.bytes().all(|byte| byte.is_ascii_digit())
                && value.parse::<u16>().is_ok()
        })
    {
        return invalid();
    }
    format!("{scheme}://{host}{port}")
}

/// Third-party errors can contain URLs, response bodies or echoed headers.
/// Copy only typed categories into our errors, and do not retain the original
/// as a source: callers may format/persist the complete anyhow source chain.
pub(super) fn transport_error(error: &minreq::Error) -> String {
    match error {
        minreq::Error::IoError(error) => format!("transport I/O error ({:?})", error.kind()),
        minreq::Error::SerdeJsonError(_) => "invalid JSON response".into(),
        minreq::Error::RustlsCreateConnection(_) => "TLS connection error".into(),
        minreq::Error::AddressNotFound => "host address not found".into(),
        minreq::Error::PunycodeConversionFailed => "invalid endpoint hostname".into(),
        minreq::Error::InvalidUtf8InBody(_) | minreq::Error::InvalidUtf8InResponse => {
            "invalid UTF-8 response".into()
        }
        minreq::Error::RedirectLocationMissing
        | minreq::Error::InfiniteRedirectionLoop
        | minreq::Error::TooManyRedirections => "HTTP redirect error".into(),
        minreq::Error::BadProxy
        | minreq::Error::BadProxyCreds
        | minreq::Error::ProxyConnect
        | minreq::Error::InvalidProxyCreds => "proxy connection or authentication error".into(),
        _ => "HTTP transport error".into(),
    }
}

#[cfg(test)]
pub(super) fn assert_safe_error(error: ProviderError, secrets: &[&str]) {
    let wrapped = anyhow::Error::new(error).context("human preparation failed");
    let mut renderings = vec![
        wrapped.to_string(),
        format!("{wrapped:#}"),
        format!("{wrapped:?}"),
        format!("{wrapped:#?}"),
    ];
    for source in wrapped.chain() {
        renderings.push(source.to_string());
        renderings.push(format!("{source:?}"));
    }
    for rendering in renderings {
        for secret in secrets {
            assert!(!rendering.contains(secret), "secret leaked: {rendering}");
        }
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

    #[test]
    fn redaction_omits_user_info_query_fragment_and_arbitrary_paths() {
        for url in [
            "https://USER_SECRET:PASS_SECRET@host.tld/arbitrary/PATH_SECRET?unknown=QUERY_SECRET#FRAGMENT_SECRET",
            "https://host.tld?unknown=QUERY_SECRET",
            "https://host.tld#FRAGMENT_SECRET",
        ] {
            assert_eq!(redact_url(url), "https://host.tld");
        }
        assert_eq!(
            redact_url("http://user:secret@[::1]:3002/private"),
            "http://[::1]:3002"
        );
        for url in [
            "secret://host/private",
            "https://host:SECRET",
            "https://host\\PATH_SECRET",
            "https://",
        ] {
            assert_eq!(redact_url(url), "<invalid-url>");
        }
    }
}
