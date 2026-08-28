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

use super::error::ProviderError;

const STOP_GAP: usize = 20;
const PARALLEL_REQUESTS: usize = 4;

// Exercise the exact vendored connector in the workspace gate, without
// running upstream's unrelated public-network integration tests.
#[cfg(test)]
#[path = "../../../../vendor/minreq/src/connect.rs"]
mod connection_tests;

/// One Esplora endpoint. The URL is display-safe (esplora auth travels in a
/// header, never the URL), so errors may show it in full.
#[derive(Clone)]
pub struct EsploraProvider {
    name: String,
    url: String,
    bearer: Option<String>,
}

/// Manual Debug: the bearer token must never reach a log line.
impl std::fmt::Debug for EsploraProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EsploraProvider")
            .field("name", &self.name)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl EsploraProvider {
    pub fn new(name: String, url: String, bearer: Option<String>) -> Self {
        EsploraProvider { name, url, bearer }
    }

    pub fn url(&self) -> &str {
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

    fn unreachable(&self, err: impl std::fmt::Display) -> ProviderError {
        ProviderError::Sync {
            url: self.url.clone(),
            message: format!("esplora unreachable: {err}"),
        }
    }

    /// The provider must serve the wallet's network: compare its genesis
    /// block hash with the expected one.
    pub fn check_network(&self, network: Network) -> Result<(), ProviderError> {
        let genesis: BlockHash = self
            .client()
            .get_block_hash(0)
            .map_err(|e| self.unreachable(e))?;
        if genesis != constants::genesis_block(network).block_hash() {
            return Err(ProviderError::WrongNetwork {
                name: self.name.clone(),
                url: self.url.clone(),
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
            .map_err(|e| self.unreachable(e))
    }

    pub fn sync(
        &self,
        request: impl Into<SyncRequest<(KeychainKind, u32)>>,
    ) -> Result<SyncResponse, ProviderError> {
        self.client()
            .sync(request, PARALLEL_REQUESTS)
            .map_err(|e| self.unreachable(e))
    }

    pub fn fee_estimates(&self) -> Result<HashMap<u16, f64>, ProviderError> {
        match self.client().get_fee_estimates() {
            Ok(estimates) => Ok(estimates),
            Err(err) => fee_estimates_from_2xx(&err).ok_or_else(|| ProviderError::Fees {
                url: self.url.clone(),
                message: format!("esplora unreachable: {err}"),
            }),
        }
    }

    pub fn broadcast(&self, tx: &Transaction) -> Result<(), ProviderError> {
        let txid = tx.compute_txid();
        match self.client().broadcast(tx) {
            Ok(()) => Ok(()),
            Err(err) if broadcast_succeeded_via_2xx(&err, &txid) => Ok(()),
            Err(e) => Err(ProviderError::Broadcast {
                url: self.url.clone(),
                message: e.to_string(),
            }),
        }
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
    use std::str::FromStr;

    use super::*;
    use crate::provider::pick_fee_rate;
    use sats_core::bitcoin::FeeRate;

    fn http_err(status: u16, message: &str) -> esplora_client::Error {
        esplora_client::Error::HttpResponse {
            status,
            message: message.into(),
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
