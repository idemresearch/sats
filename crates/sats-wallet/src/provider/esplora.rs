//! Esplora REST driver: chain sync, fee estimation, broadcast.
//!
//! Carries the deployment quirks of real-world Esplora instances: some
//! deployments rewrite 200 responses into other 2xx statuses (mempool.space's
//! signet `/fee-estimates` returns 203) while the client rejects anything
//! that isn't exactly 200 — the helpers below recover the still-valid bodies.

use std::collections::HashMap;

use bdk_esplora::EsploraExt;
use bdk_esplora::esplora_client::{self, BlockingClient};
use bdk_wallet::KeychainKind;
use bdk_wallet::chain::spk_client::{FullScanRequest, FullScanResponse, SyncRequest, SyncResponse};
use sats_core::bitcoin::{BlockHash, Network, Transaction, Txid, constants};

use super::error::{ProviderError, redact_url, transport_error};

const STOP_GAP: usize = 20;
const PARALLEL_REQUESTS: usize = 4;

// Exercise the exact vendored connector in the workspace gate, without
// running upstream's unrelated public-network integration tests.
#[cfg(test)]
#[path = "../../../../vendor/minreq/src/connect.rs"]
mod connection_tests;

/// Keep the configured endpoint private; arbitrary deployments may carry
/// credentials in any URL component as well as the Authorization header.
#[derive(Clone)]
pub struct EsploraProvider {
    name: String,
    url: String,
    display_url: String,
    bearer: Option<String>,
}

/// Manual Debug: the bearer token must never reach a log line.
impl std::fmt::Debug for EsploraProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EsploraProvider")
            .field("name", &self.name)
            .field("url", &self.display_url)
            .finish_non_exhaustive()
    }
}

impl EsploraProvider {
    pub fn new(name: String, url: String, bearer: Option<String>) -> Self {
        let display_url = redact_url(&url);
        EsploraProvider {
            name,
            url,
            display_url,
            bearer,
        }
    }

    pub fn display_url(&self) -> &str {
        &self.display_url
    }

    #[cfg(test)]
    pub(super) fn url(&self) -> &str {
        &self.url
    }

    fn client(&self) -> BlockingClient {
        let mut builder = esplora_client::Builder::new(&self.url)
            .timeout(super::HTTP_TIMEOUT_SECS)
            .max_retries(2);
        if let Some(token) = &self.bearer {
            builder = builder.header("Authorization", &format!("Bearer {token}"));
        }
        builder.build_blocking()
    }

    fn unreachable(&self, err: &esplora_client::Error) -> ProviderError {
        ProviderError::Sync {
            url: self.display_url.clone(),
            message: format!("esplora: {}", error_message(err)),
        }
    }

    /// The provider must serve the wallet's network: compare its genesis
    /// block hash with the expected one.
    pub fn check_network(&self, network: Network) -> Result<(), ProviderError> {
        let genesis: BlockHash = self
            .client()
            .get_block_hash(0)
            .map_err(|e| self.unreachable(&e))?;
        if genesis != constants::genesis_block(network).block_hash() {
            return Err(ProviderError::WrongNetwork {
                name: self.name.clone(),
                url: self.display_url.clone(),
                expected: crate::config::network_name(network),
            });
        }
        Ok(())
    }

    pub fn full_scan(
        &self,
        request: impl Into<FullScanRequest<KeychainKind>>,
    ) -> Result<FullScanResponse<KeychainKind>, ProviderError> {
        self.client()
            .full_scan(request, STOP_GAP, PARALLEL_REQUESTS)
            .map_err(|e| self.unreachable(&e))
    }

    pub fn sync(
        &self,
        request: impl Into<SyncRequest<(KeychainKind, u32)>>,
    ) -> Result<SyncResponse, ProviderError> {
        self.client()
            .sync(request, PARALLEL_REQUESTS)
            .map_err(|e| self.unreachable(&e))
    }

    pub fn fee_estimates(&self) -> Result<HashMap<u16, f64>, ProviderError> {
        match self.client().get_fee_estimates() {
            Ok(estimates) => Ok(estimates),
            Err(err) => fee_estimates_from_2xx(&err).ok_or_else(|| ProviderError::Fees {
                url: self.display_url.clone(),
                message: format!("esplora: {}", error_message(&err)),
            }),
        }
    }

    pub fn broadcast(&self, tx: &Transaction) -> Result<(), ProviderError> {
        let txid = tx.compute_txid();
        match self.broadcast_request(tx) {
            Ok(()) => Ok(()),
            Err(err) if broadcast_succeeded_via_2xx(&err, &txid) => Ok(()),
            Err(e) => Err(ProviderError::Broadcast {
                url: self.display_url.clone(),
                message: format!("esplora: {}", error_message(&e)),
            }),
        }
    }

    /// esplora-client 0.12's blocking POST omits configured headers. Use the
    /// same transport and response contract here so private relays receive
    /// their bearer token too. Never retry a broadcast after an uncertain
    /// response; the caller has already persisted the signed transaction.
    fn broadcast_request(&self, tx: &Transaction) -> Result<(), esplora_client::Error> {
        let mut request = minreq::post(format!("{}/tx", self.url))
            .with_timeout(super::HTTP_TIMEOUT_SECS)
            .with_body(hex::encode(sats_core::bitcoin::consensus::serialize(tx)));
        if let Some(token) = &self.bearer {
            request = request.with_header("Authorization", format!("Bearer {token}"));
        }
        let response = request.send()?;
        if response.status_code == 200 {
            return Ok(());
        }
        let status =
            u16::try_from(response.status_code).map_err(esplora_client::Error::StatusCode)?;
        Err(esplora_client::Error::HttpResponse {
            status,
            message: response.as_str().unwrap_or_default().to_string(),
        })
    }
}

/// Do not format the library's Display/Debug: both can include credentials
/// echoed in HTTP responses, header validation or nested transport errors.
fn error_message(error: &esplora_client::Error) -> String {
    use esplora_client::Error;
    match error {
        Error::Minreq(error) => transport_error(error),
        Error::HttpResponse { status, .. } => format!("http {status}"),
        Error::Parsing(_) => "invalid numeric response".into(),
        Error::StatusCode(_) => "invalid HTTP status".into(),
        Error::BitcoinEncoding(_) | Error::HexToArray(_) | Error::HexToBytes(_) => {
            "invalid Bitcoin response data".into()
        }
        Error::TransactionNotFound(_) => "transaction not found".into(),
        Error::HeaderHeightNotFound(_) | Error::HeaderHashNotFound(_) => {
            "block header not found".into()
        }
        Error::InvalidHttpHeaderName(_) | Error::InvalidHttpHeaderValue(_) => {
            "invalid HTTP header configuration".into()
        }
        Error::InvalidResponse => "invalid HTTP response".into(),
    }
}

/// The body of a rewritten-2xx `/fee-estimates` response is still the
/// fee-estimate map; recover it.
fn fee_estimates_from_2xx(err: &esplora_client::Error) -> Option<HashMap<u16, f64>> {
    match err {
        esplora_client::Error::HttpResponse { status, message } if (200..300).contains(status) => {
            serde_json::from_str(message).ok()
        }
        _ => None,
    }
}

/// The 2xx-rewrite case for `POST /tx`: the server accepted the transaction
/// iff the body it echoed back is our txid.
fn broadcast_succeeded_via_2xx(err: &esplora_client::Error, txid: &Txid) -> bool {
    matches!(err, esplora_client::Error::HttpResponse { status, message }
        if (200..300).contains(status) && message.trim() == txid.to_string())
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, Read, Write};
    use std::net::TcpListener;
    use std::str::FromStr;
    use std::sync::mpsc;

    use super::*;
    use crate::provider::pick_fee_rate;
    use sats_core::bitcoin::FeeRate;

    fn http_err(status: u16, message: &str) -> esplora_client::Error {
        esplora_client::Error::HttpResponse {
            status,
            message: message.into(),
        }
    }

    fn local_server(responses: Vec<(u16, String)>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (send, received) = mpsc::channel();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                let mut content_length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    request.push_str(&line);
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse().unwrap();
                    }
                }
                let mut body_bytes = vec![0; content_length];
                reader.read_exact(&mut body_bytes).unwrap();
                request.push_str(&String::from_utf8(body_bytes).unwrap());
                send.send(request).unwrap();
                write!(stream, "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        (origin, received)
    }

    #[test]
    fn raw_library_errors_never_survive_wrapping() {
        let secrets = [
            "USERSECRET",
            "PASSSECRET",
            "PATHSECRET",
            "QUERYSECRET",
            "FRAGMENTSECRET",
            "BEARERSECRET",
        ];
        let provider = EsploraProvider::new(
            "fixture".into(),
            "https://USERSECRET:PASSSECRET@host.invalid/PATHSECRET?unknown=QUERYSECRET#FRAGMENTSECRET".into(),
            Some("BEARERSECRET".into()),
        );
        for secret in secrets {
            assert!(!format!("{provider:#?}").contains(secret));
        }
        let echo = format!("{} Authorization: Bearer BEARERSECRET", provider.url);
        for error in [
            http_err(401, &echo),
            esplora_client::Error::InvalidHttpHeaderValue(echo.clone()),
            esplora_client::Error::Minreq(minreq::Error::IoError(std::io::Error::other(
                echo.clone(),
            ))),
            esplora_client::Error::Minreq(minreq::Error::SerdeJsonError(
                serde_json::from_value::<u64>(serde_json::json!(echo)).unwrap_err(),
            )),
        ] {
            super::super::error::assert_safe_error(provider.unreachable(&error), &secrets);
        }
    }

    #[test]
    fn endpoint_and_bearer_are_used_but_never_echoed_from_http_failures() {
        let echo = "PATHSECRET QUERYSECRET BEARERSECRET USERSECRET";
        let (origin, requests) = local_server(vec![
            (200, r#"{"2":3.0}"#.into()),
            (401, echo.into()),
            (401, echo.into()),
            (401, echo.into()),
        ]);
        let provider = EsploraProvider::new(
            "fixture".into(),
            format!("{origin}/arbitrary/PATHSECRET?unknown=QUERYSECRET"),
            Some("BEARERSECRET".into()),
        );
        assert_eq!(provider.fee_estimates().unwrap().get(&2), Some(&3.0));
        let tx = Transaction {
            version: sats_core::bitcoin::transaction::Version::TWO,
            lock_time: sats_core::bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![],
        };
        for error in [
            provider.check_network(Network::Signet).unwrap_err(),
            provider.fee_estimates().unwrap_err(),
            provider.broadcast(&tx).unwrap_err(),
        ] {
            super::super::error::assert_safe_error(
                error,
                &echo.split_whitespace().collect::<Vec<_>>(),
            );
        }
        let requests: Vec<_> = requests.into_iter().collect();
        assert_eq!(requests.len(), 4);
        for (request, operation) in
            requests
                .iter()
                .zip(["fee-estimates", "block-height/0", "fee-estimates", "tx"])
        {
            assert!(request.lines().next().unwrap().contains(&format!(
                "/arbitrary/PATHSECRET?unknown=QUERYSECRET/{operation}"
            )));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer bearersecret")
            );
        }
    }

    #[test]
    fn authenticated_broadcast_preserves_bytes_2xx_semantics_and_does_not_retry() {
        let tx = Transaction {
            version: sats_core::bitcoin::transaction::Version::TWO,
            lock_time: sats_core::bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![],
        };
        let (origin, requests) = local_server(vec![
            (200, String::new()),
            (203, format!("{}\n", tx.compute_txid())),
            (203, "accepted".into()),
            (500, "BEARERSECRET".into()),
        ]);
        let provider = EsploraProvider::new(
            "fixture".into(),
            format!("{origin}/api"),
            Some("BEARERSECRET".into()),
        );
        provider.broadcast(&tx).unwrap();
        provider.broadcast(&tx).unwrap();
        assert!(provider.broadcast(&tx).is_err());
        super::super::error::assert_safe_error(
            provider.broadcast(&tx).unwrap_err(),
            &["BEARERSECRET"],
        );
        let requests: Vec<_> = requests.into_iter().collect();
        assert_eq!(requests.len(), 4);
        for request in requests {
            assert!(request.starts_with("POST /api/tx HTTP/1.1\r\n"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer bearersecret")
            );
            assert_eq!(
                request.split_once("\r\n\r\n").unwrap().1,
                hex::encode(sats_core::bitcoin::consensus::serialize(&tx))
            );
        }
    }

    // The response mempool.space's signet endpoint served with status 203:
    // valid estimates behind a non-200 status.
    const SIGNET_203_BODY: &str =
        r#"{"1":3.501,"2":3.501,"3":3.251,"25":3.001,"144":0.2,"504":0.2,"1008":0.1}"#;

    #[test]
    fn recovers_fee_estimates_from_203() {
        let estimates =
            fee_estimates_from_2xx(&http_err(203, SIGNET_203_BODY)).expect("2xx body parses");
        assert_eq!(
            pick_fee_rate(&estimates, 2).unwrap(),
            FeeRate::from_sat_per_vb_u32(4)
        );
    }

    #[test]
    fn real_errors_stay_errors() {
        assert!(fee_estimates_from_2xx(&http_err(404, "not found")).is_none());
        assert!(fee_estimates_from_2xx(&http_err(500, SIGNET_203_BODY)).is_none());
        assert!(fee_estimates_from_2xx(&http_err(203, "<html>portal</html>")).is_none());
    }

    #[test]
    fn broadcast_2xx_requires_echoed_txid() {
        let txid =
            Txid::from_str("aa00000000000000000000000000000000000000000000000000000000000000")
                .unwrap();
        assert!(broadcast_succeeded_via_2xx(
            &http_err(203, &format!("{txid}\n")),
            &txid
        ));
        assert!(!broadcast_succeeded_via_2xx(
            &http_err(203, "accepted"),
            &txid
        ));
        assert!(!broadcast_succeeded_via_2xx(
            &http_err(400, &txid.to_string()),
            &txid
        ));
    }
}
