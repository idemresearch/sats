//! Persisted wallet + esplora chain access for the CLI.

use std::collections::HashMap;
use std::fs;
use std::io::{IsTerminal, Write};

use anyhow::{Context, Result, bail};
use bdk_esplora::EsploraExt;
use bdk_esplora::esplora_client::{self, BlockingClient};
use bdk_wallet::{PersistedWallet, Wallet};
use rusqlite::Connection;
use sats_core::bitcoin::{FeeRate, Network, Transaction, Txid};

use crate::config::{Config, network_name};
use crate::store::Store;

const STOP_GAP: usize = 20;
const PARALLEL_REQUESTS: usize = 4;

pub struct WalletCtx {
    pub wallet: PersistedWallet<Connection>,
    pub conn: Connection,
    pub network: Network,
    pub net_name: &'static str,
    pub esplora_url: String,
}

/// Create the persisted watch-only wallet for a network. Public descriptors
/// only — the sqlite file never holds key material.
pub fn create(store: &Store, network: Network, ext: String, int: String) -> Result<()> {
    let net_name = network_name(network);
    let db = store.wallet_db_path(net_name);
    if db.exists() {
        bail!("wallet already exists for {net_name} ({})", db.display());
    }
    let dir = db.parent().context("bad wallet path")?;
    fs::create_dir_all(dir)?;
    let mut conn = Connection::open(&db)?;
    Wallet::create(ext, int)
        .network(network)
        .create_wallet(&mut conn)
        .context("cannot create wallet")?;
    Ok(())
}

pub fn open(store: &Store, config: &Config, network: Network) -> Result<WalletCtx> {
    let net_name = network_name(network);
    let db = store.wallet_db_path(net_name);
    if !db.exists() {
        bail!("no {net_name} wallet — run: sats init --network {net_name}");
    }
    let mut conn = Connection::open(&db)?;
    let wallet = Wallet::load()
        .check_network(network)
        .load_wallet(&mut conn)
        .context("cannot load wallet")?
        .with_context(|| format!("empty wallet database {}", db.display()))?;
    Ok(WalletCtx {
        wallet,
        conn,
        network,
        net_name,
        esplora_url: config.esplora_url(net_name)?,
    })
}

impl WalletCtx {
    pub fn persist(&mut self) -> Result<()> {
        self.wallet.persist(&mut self.conn)?;
        Ok(())
    }

    fn client(&self) -> Result<BlockingClient> {
        Ok(esplora_client::Builder::new(&self.esplora_url).build_blocking())
    }

    /// Sync against esplora: a full scan on first touch, incremental after.
    pub fn sync(&mut self) -> Result<()> {
        let client = self.client()?;
        let status = StatusLine::start("syncing…");
        let result = (|| -> Result<()> {
            if self.wallet.latest_checkpoint().height() == 0 {
                let request = self.wallet.start_full_scan();
                let update = client
                    .full_scan(request, STOP_GAP, PARALLEL_REQUESTS)
                    .map_err(|e| {
                        anyhow::anyhow!("esplora unreachable ({}): {e}", self.esplora_url)
                    })?;
                self.wallet.apply_update(update)?;
            } else {
                let request = self.wallet.start_sync_with_revealed_spks();
                let update = client.sync(request, PARALLEL_REQUESTS).map_err(|e| {
                    anyhow::anyhow!("esplora unreachable ({}): {e}", self.esplora_url)
                })?;
                self.wallet.apply_update(update)?;
            }
            self.persist()
        })();
        status.finish();
        result
    }

    /// Estimated fee rate for confirmation within `target` blocks, floored
    /// at 1 sat/vB.
    pub fn estimate_fee_rate(&self, target: u16) -> Result<FeeRate> {
        let estimates = match self.client()?.get_fee_estimates() {
            Ok(estimates) => estimates,
            Err(err) => fee_estimates_from_2xx(&err).ok_or_else(|| {
                anyhow::anyhow!("esplora unreachable ({}): {err}", self.esplora_url)
            })?,
        };
        Ok(pick_fee_rate(&estimates, target))
    }

    pub fn broadcast(&mut self, tx: &Transaction) -> Result<Txid> {
        let txid = tx.compute_txid();
        match self.client()?.broadcast(tx) {
            Ok(()) => {}
            Err(err) if broadcast_succeeded_via_2xx(&err, &txid) => {}
            Err(e) => bail!("broadcast failed ({}): {e}", self.esplora_url),
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.wallet.apply_unconfirmed_txs([(tx.clone(), now)]);
        self.persist()?;
        Ok(txid)
    }
}

/// Esplora proper answers 200, but some deployments rewrite success into
/// another 2xx — mempool.space's signet `/fee-estimates` returns 203 — and
/// the client rejects anything that isn't exactly 200. The body of such a
/// response is still the fee-estimate map, so recover it.
fn fee_estimates_from_2xx(err: &esplora_client::Error) -> Option<HashMap<u16, f64>> {
    match err {
        esplora_client::Error::HttpResponse { status, message } if (200..300).contains(status) => {
            serde_json::from_str(message).ok()
        }
        _ => None,
    }
}

/// Largest conf target ≤ the requested one; else the closest above.
fn pick_fee_rate(estimates: &HashMap<u16, f64>, target: u16) -> FeeRate {
    let sat_vb = estimates
        .iter()
        .filter(|(k, _)| **k <= target)
        .max_by_key(|(k, _)| **k)
        .or_else(|| estimates.iter().min_by_key(|(k, _)| **k))
        .map(|(_, rate)| *rate)
        .unwrap_or(1.0);
    FeeRate::from_sat_per_vb_u32((sat_vb.ceil() as u32).max(1))
}

/// The 2xx-rewrite case for `POST /tx`: the server accepted the transaction
/// iff the body it echoed back is our txid.
fn broadcast_succeeded_via_2xx(err: &esplora_client::Error, txid: &Txid) -> bool {
    matches!(err, esplora_client::Error::HttpResponse { status, message }
        if (200..300).contains(status) && message.trim() == txid.to_string())
}

/// A transient stderr status line, erased when the work finishes.
struct StatusLine {
    active: bool,
    len: usize,
}

impl StatusLine {
    fn start(msg: &str) -> StatusLine {
        let active = std::io::stderr().is_terminal();
        if active {
            eprint!("{msg}");
            let _ = std::io::stderr().flush();
        }
        StatusLine {
            active,
            len: msg.chars().count(),
        }
    }

    fn finish(self) {
        if self.active {
            eprint!("\r{}\r", " ".repeat(self.len));
            let _ = std::io::stderr().flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

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
            pick_fee_rate(&estimates, 2),
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
    fn fee_rate_targets_then_floors() {
        let estimates = HashMap::from([(1u16, 5.0), (3, 2.2), (25, 1.0)]);
        // Exact / nearest-below target, ceiled.
        assert_eq!(
            pick_fee_rate(&estimates, 3),
            FeeRate::from_sat_per_vb_u32(3)
        );
        assert_eq!(
            pick_fee_rate(&estimates, 2),
            FeeRate::from_sat_per_vb_u32(5)
        );
        // Nothing at or below the target: closest above.
        let slow_only = HashMap::from([(6u16, 2.0)]);
        assert_eq!(
            pick_fee_rate(&slow_only, 2),
            FeeRate::from_sat_per_vb_u32(2)
        );
        // No estimates at all: 1 sat/vB floor.
        assert_eq!(
            pick_fee_rate(&HashMap::new(), 2),
            FeeRate::from_sat_per_vb_u32(1)
        );
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
