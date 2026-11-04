//! HTTP endpoints for the relay's OAuth flow (see [`crate::oauth`]).

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use sats_core::relay::Capability;
use serde::Deserialize;

use crate::oauth::{self, Kind, Record};
use crate::{Body, Relay, now, respond};

/// The largest form or registration body accepted.
const MAX_FORM_BODY: usize = 64 * 1024;

/// The relay's own origin, as the client reached it. Behind a TLS proxy the
/// scheme comes from `X-Forwarded-Proto`; otherwise loopback is plain HTTP
/// and anything else HTTPS.
pub fn origin(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    if host.is_empty() || host.contains(['/', ' ', '@', '\\']) {
        return None;
    }
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.split_once(']').map(|(h, _)| h))
        .unwrap_or_else(|| host.rsplit_once(':').map_or(host, |(h, _)| h));
    let loopback = bare == "localhost"
        || bare
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|p| p.to_str().ok())
        .map(|p| p.split(',').next().unwrap_or_default().trim())
        .filter(|p| *p == "https" || *p == "http")
        .unwrap_or(if loopback { "http" } else { "https" });
    Some(format!("{proto}://{host}"))
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response<Body> {
    let mut response = respond(
        status,
        serde_json::json!({"error": error, "error_description": description}),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn bad_origin() -> Response<Body> {
    oauth_error(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "missing or invalid Host header",
    )
}

fn store_failed(error: anyhow::Error) -> Response<Body> {
    eprintln!("sats-relay: oauth store error: {error:#}");
    oauth_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "the relay's store failed",
    )
}

pub fn protected_resource(headers: &HeaderMap) -> Response<Body> {
    let Some(origin) = origin(headers) else {
        return bad_origin();
    };
    respond(
        StatusCode::OK,
        serde_json::json!({
            "resource": format!("{origin}/mcp"),
            "authorization_servers": [origin],
            "bearer_methods_supported": ["header"],
            "scopes_supported": ["sats"],
            "resource_name": "sats relay",
        }),
    )
}

pub fn authorization_server(headers: &HeaderMap) -> Response<Body> {
    let Some(origin) = origin(headers) else {
        return bad_origin();
    };
    respond(
        StatusCode::OK,
        serde_json::json!({
            "issuer": origin,
            "authorization_endpoint": format!("{origin}/oauth/authorize"),
            "token_endpoint": format!("{origin}/oauth/token"),
            "registration_endpoint": format!("{origin}/oauth/register"),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"],
            "scopes_supported": ["sats"],
        }),
    )
}

async fn read_body(request: Request<Incoming>) -> Option<(hyper::http::request::Parts, Bytes)> {
    let (parts, body) = request.into_parts();
    let body = Limited::new(body, MAX_FORM_BODY).collect().await.ok()?;
    Some((parts, body.to_bytes()))
}

#[derive(Deserialize)]
struct Registration {
    redirect_uris: Vec<String>,
}

pub async fn register(relay: &Relay, request: Request<Incoming>) -> Response<Body> {
    let Some((_, body)) = read_body(request).await else {
        return oauth_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request",
            "body too large",
        );
    };
    let Ok(registration) = serde_json::from_slice::<Registration>(&body) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "expected JSON with redirect_uris",
        );
    };
    match relay.oauth.register(&registration.redirect_uris, now()) {
        Ok(Some(client_id)) => respond(
            StatusCode::CREATED,
            serde_json::json!({
                "client_id": client_id,
                "client_id_issued_at": now(),
                "redirect_uris": registration.redirect_uris,
                "token_endpoint_auth_method": "none",
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
            }),
        ),
        Ok(None) => oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "redirect URIs must be HTTPS (or HTTP to loopback), without fragments",
        ),
        Err(e) => store_failed(e),
    }
}

/// The authorization request's parameters, as validated.
struct AuthorizeParams {
    client_id: String,
    redirect_uri: String,
    state: Option<String>,
    code_challenge: String,
}

fn field<'a>(form: &'a [(String, String)], name: &str) -> Option<&'a str> {
    form.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// Validate an authorization request. Errors are shown on the page rather
/// than redirected: a request that fails here never names a trusted
/// redirect target.
fn authorize_params(relay: &Relay, form: &[(String, String)]) -> Result<AuthorizeParams, String> {
    let client_id = field(form, "client_id").ok_or("missing client_id")?;
    let redirect_uri = field(form, "redirect_uri").ok_or("missing redirect_uri")?;
    match relay.oauth.redirect_allowed(client_id, redirect_uri) {
        Ok(true) => {}
        Ok(false) => return Err("unknown client or unregistered redirect_uri".into()),
        Err(e) => return Err(format!("the relay's store failed: {e}")),
    }
    if field(form, "response_type") != Some("code") {
        return Err("response_type must be code".into());
    }
    if field(form, "code_challenge_method") != Some("S256") {
        return Err("PKCE with code_challenge_method=S256 is required".into());
    }
    let code_challenge = field(form, "code_challenge").ok_or("missing code_challenge")?;
    if code_challenge.len() != 43 {
        return Err("malformed code_challenge".into());
    }
    Ok(AuthorizeParams {
        client_id: client_id.into(),
        redirect_uri: redirect_uri.into(),
        state: field(form, "state").map(Into::into),
        code_challenge: code_challenge.into(),
    })
}

fn html(status: StatusCode, body: String) -> Response<Body> {
    let mut response = Response::new(Full::new(Bytes::from(body)).boxed());
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
        ),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
}

const STYLE: &str = "body{font:16px/1.5 -apple-system,system-ui,sans-serif;max-width:34rem;\
margin:2rem auto;padding:0 1rem;color:#111;background:#fff}\
@media(prefers-color-scheme:dark){body{color:#eee;background:#111}textarea{background:#222;color:#eee}}\
textarea{width:100%;box-sizing:border-box;height:7rem;font:13px ui-monospace,monospace}\
button{font-size:1rem;padding:.6rem 1.2rem;margin-top:.8rem}.error{color:#c0392b}code{font-size:.9em}";

fn page(title: &str, inner: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{}</title><style>{STYLE}</style></head><body>{inner}</body></html>",
        oauth::html_escape(title)
    )
}

fn error_page(message: &str) -> Response<Body> {
    html(
        StatusCode::BAD_REQUEST,
        page(
            "Can't connect",
            &format!(
                "<h1>Can't connect</h1><p class=error>{}</p>",
                oauth::html_escape(message)
            ),
        ),
    )
}

fn consent_page(
    form: &[(String, String)],
    params: &AuthorizeParams,
    error: Option<&str>,
) -> String {
    let app = params
        .redirect_uri
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split(['/', '?']).next())
        .unwrap_or("An app");
    let hidden: String = [
        "client_id",
        "redirect_uri",
        "response_type",
        "state",
        "code_challenge",
        "code_challenge_method",
        "resource",
        "scope",
    ]
    .iter()
    .filter_map(|name| {
        field(form, name).map(|value| {
            format!(
                "<input type=hidden name={name} value=\"{}\">",
                oauth::html_escape(value)
            )
        })
    })
    .collect();
    let error = error
        .map(|e| format!("<p class=error>{}</p>", oauth::html_escape(e)))
        .unwrap_or_default();
    page(
        "Connect to sats",
        &format!(
            "<h1>Connect to your sats wallet</h1>\
             <p><strong>{app}</strong> wants to file requests to your wallet through this relay. \
             It can only ask: every request waits for your approval on your own device, and it \
             can never sign or spend.</p>\
             <p>Paste the connection code that <code>sats agent grant &lt;name&gt; --relay</code> \
             printed on your device.</p>{error}\
             <form method=post action=/oauth/authorize>{hidden}\
             <textarea name=code required autocomplete=off autocapitalize=off spellcheck=false \
             placeholder=\"connection code\"></textarea>\
             <button type=submit>Connect</button></form>",
            app = oauth::html_escape(app),
        ),
    )
}

/// The code the human pasted, without surrounding whitespace or quotes.
fn pasted_capability(pasted: &str) -> &str {
    pasted.trim().trim_matches(['"', '\'', '<', '>']).trim()
}

pub async fn authorize(relay: &Relay, request: Request<Incoming>) -> Response<Body> {
    let is_post = request.method() == Method::POST;
    let query = request.uri().query().unwrap_or_default().to_string();
    let Some((_, body)) = read_body(request).await else {
        return error_page("request too large");
    };
    let form = if is_post {
        oauth::parse_form(&String::from_utf8_lossy(&body))
    } else {
        oauth::parse_form(&query)
    };
    let params = match authorize_params(relay, &form) {
        Ok(params) => params,
        Err(message) => return error_page(&message),
    };
    if !is_post {
        return html(StatusCode::OK, consent_page(&form, &params, None));
    }
    let pasted = field(&form, "code").unwrap_or_default();
    let encoded = pasted_capability(pasted);
    let capability = match Capability::decode(encoded) {
        Ok(capability) if now() < capability.expires_at => capability,
        Ok(_) => {
            return html(
                StatusCode::BAD_REQUEST,
                consent_page(
                    &form,
                    &params,
                    Some(
                        "That code's grant has expired. Issue a new one with sats agent grant <name> --relay.",
                    ),
                ),
            );
        }
        Err(_) => {
            return html(
                StatusCode::BAD_REQUEST,
                consent_page(
                    &form,
                    &params,
                    Some("That isn't a valid connection code. Paste the code sats printed, whole."),
                ),
            );
        }
    };
    let record = Record {
        capability: encoded.to_string(),
        client_id: params.client_id.clone(),
        redirect_uri: Some(params.redirect_uri.clone()),
        code_challenge: Some(params.code_challenge.clone()),
    };
    let code = match relay.oauth.issue(
        Kind::Code,
        &record,
        oauth::CODE_TTL.min(capability.expires_at - now()),
        now(),
    ) {
        Ok(code) => code,
        Err(e) => return store_failed(e),
    };
    let separator = if params.redirect_uri.contains('?') {
        '&'
    } else {
        '?'
    };
    let mut location = format!(
        "{}{separator}code={}",
        params.redirect_uri,
        oauth::encode(&code)
    );
    if let Some(state) = &params.state {
        location.push_str(&format!("&state={}", oauth::encode(state)));
    }
    let mut response = Response::new(Full::new(Bytes::new()).boxed());
    *response.status_mut() = StatusCode::FOUND;
    match HeaderValue::from_str(&location) {
        Ok(value) => {
            response.headers_mut().insert(header::LOCATION, value);
        }
        Err(_) => return error_page("the redirect URI can't be used"),
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Issue an access and refresh token pair for a live capability.
fn token_pair(relay: &Relay, capability_text: &str, client_id: &str) -> Response<Body> {
    let capability = match Capability::decode(capability_text) {
        Ok(capability) if now() < capability.expires_at => capability,
        _ => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "the grant behind this connection has expired or is invalid",
            );
        }
    };
    let remaining = capability.expires_at - now();
    let record = Record {
        capability: capability_text.to_string(),
        client_id: client_id.to_string(),
        redirect_uri: None,
        code_challenge: None,
    };
    let access_ttl = oauth::ACCESS_TTL.min(remaining);
    let issued = relay
        .oauth
        .issue(Kind::Access, &record, access_ttl, now())
        .and_then(|access| {
            relay
                .oauth
                .issue(
                    Kind::Refresh,
                    &record,
                    oauth::REFRESH_TTL.min(remaining),
                    now(),
                )
                .map(|refresh| (access, refresh))
        });
    let (access, refresh) = match issued {
        Ok(pair) => pair,
        Err(e) => return store_failed(e),
    };
    let mut response = respond(
        StatusCode::OK,
        serde_json::json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": access_ttl,
            "refresh_token": refresh,
            "scope": "sats",
        }),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub async fn token(relay: &Relay, request: Request<Incoming>) -> Response<Body> {
    let Some((_, body)) = read_body(request).await else {
        return oauth_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request",
            "body too large",
        );
    };
    let form = oauth::parse_form(&String::from_utf8_lossy(&body));
    let client_id = field(&form, "client_id").unwrap_or_default();
    let invalid_grant = |why: &str| oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", why);
    match field(&form, "grant_type") {
        Some("authorization_code") => {
            let code = field(&form, "code").unwrap_or_default();
            let verifier = field(&form, "code_verifier").unwrap_or_default();
            let record = match relay.oauth.redeem(Kind::Code, code, now(), true) {
                Ok(Some(record)) => record,
                Ok(None) => return invalid_grant("unknown, used, or expired code"),
                Err(e) => return store_failed(e),
            };
            if record.client_id != client_id
                || record.redirect_uri.as_deref() != field(&form, "redirect_uri")
            {
                return invalid_grant("the code was issued to another client or redirect_uri");
            }
            let challenge = record.code_challenge.as_deref().unwrap_or_default();
            if !oauth::valid_verifier(verifier)
                || !bool::from(subtle::ConstantTimeEq::ct_eq(
                    oauth::pkce_challenge(verifier).as_bytes(),
                    challenge.as_bytes(),
                ))
            {
                return invalid_grant("PKCE verification failed");
            }
            token_pair(relay, &record.capability, client_id)
        }
        Some("refresh_token") => {
            let refresh = field(&form, "refresh_token").unwrap_or_default();
            let record = match relay.oauth.redeem(Kind::Refresh, refresh, now(), true) {
                Ok(Some(record)) => record,
                Ok(None) => return invalid_grant("unknown, used, or expired refresh token"),
                Err(e) => return store_failed(e),
            };
            if record.client_id != client_id {
                return invalid_grant("the refresh token was issued to another client");
            }
            token_pair(relay, &record.capability, client_id)
        }
        _ => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "use authorization_code or refresh_token",
        ),
    }
}

/// A 401 that points the client at the relay's OAuth metadata.
pub fn unauthorized(headers: &HeaderMap, token_given: bool) -> Response<Body> {
    let metadata = origin(headers)
        .map(|origin| format!("{origin}/.well-known/oauth-protected-resource"))
        .unwrap_or_else(|| "/.well-known/oauth-protected-resource".into());
    let mut challenge = format!("Bearer resource_metadata=\"{metadata}\", scope=\"sats\"");
    if token_given {
        challenge.push_str(", error=\"invalid_token\"");
    }
    let mut response = oauth_error(
        StatusCode::UNAUTHORIZED,
        if token_given {
            "invalid_token"
        } else {
            "unauthorized"
        },
        "sign in to this relay with OAuth",
    );
    if let Ok(value) = HeaderValue::from_str(&challenge) {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, value);
    }
    response
}

/// The capability behind an `Authorization: Bearer` access token.
pub fn bearer_capability(relay: &Relay, headers: &HeaderMap) -> Result<Capability, bool> {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Err(false);
    };
    let token = value
        .to_str()
        .ok()
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(str::trim)
        .ok_or(true)?;
    let record = relay
        .oauth
        .redeem(Kind::Access, token, now(), false)
        .ok()
        .flatten()
        .ok_or(true)?;
    match Capability::decode(&record.capability) {
        Ok(capability) if now() < capability.expires_at => Ok(capability),
        _ => Err(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn the_origin_follows_the_proxy_and_defaults_by_host() {
        assert_eq!(
            origin(&headers(&[("host", "relay.example.com")])).unwrap(),
            "https://relay.example.com"
        );
        assert_eq!(
            origin(&headers(&[("host", "127.0.0.1:8080")])).unwrap(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            origin(&headers(&[
                ("host", "relay.example.com"),
                ("x-forwarded-proto", "https, http")
            ]))
            .unwrap(),
            "https://relay.example.com"
        );
        assert_eq!(
            origin(&headers(&[
                ("host", "localhost:1"),
                ("x-forwarded-proto", "gopher")
            ]))
            .unwrap(),
            "http://localhost:1"
        );
        assert!(origin(&headers(&[("host", "a/b")])).is_none());
        assert!(origin(&HeaderMap::new()).is_none());
    }

    #[test]
    fn a_pasted_code_is_trimmed_of_whitespace_and_quotes() {
        assert_eq!(pasted_capability("  abc.def \n"), "abc.def");
        assert_eq!(pasted_capability("abc.def"), "abc.def");
        assert_eq!(pasted_capability("\"x.y\""), "x.y");
    }
}
