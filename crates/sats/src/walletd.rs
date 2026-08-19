//! Persisted wallet + esplora chain access for the CLI.

use std::fs;
use std::io::{IsTerminal, Write};

use anyhow::{Context, Result, bail};
use bdk_esplora::EsploraExt;
use bdk_esplora::esplora_client::{self, BlockingClient};
use bdk_wallet::rusqlite::Connection;
use bdk_wallet::{PersistedWallet, Wallet};
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
                    .map_err(|e| anyhow::anyhow!("esplora unreachable ({}): {e}", self.esplora_url))?;
                self.wallet.apply_update(update)?;
            } else {
                let request = self.wallet.start_sync_with_revealed_spks();
                let update = client
                    .sync(request, PARALLEL_REQUESTS)
                    .map_err(|e| anyhow::anyhow!("esplora unreachable ({}): {e}", self.esplora_url))?;
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
        let estimates = self
            .client()?
            .get_fee_estimates()
            .map_err(|e| anyhow::anyhow!("esplora unreachable ({}): {e}", self.esplora_url))?;
        // Largest conf target ≤ the requested one; else the closest above.
        let sat_vb = estimates
            .iter()
            .filter(|(k, _)| **k <= target)
            .max_by_key(|(k, _)| **k)
            .or_else(|| estimates.iter().min_by_key(|(k, _)| **k))
            .map(|(_, rate)| *rate)
            .unwrap_or(1.0);
        Ok(FeeRate::from_sat_per_vb_u32((sat_vb.ceil() as u32).max(1)))
    }

    pub fn broadcast(&mut self, tx: &Transaction) -> Result<Txid> {
        self.client()?
            .broadcast(tx)
            .map_err(|e| anyhow::anyhow!("broadcast failed ({}): {e}", self.esplora_url))?;
        let txid = tx.compute_txid();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.wallet.apply_unconfirmed_txs([(tx.clone(), now)]);
        self.persist()?;
        Ok(txid)
    }
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
        StatusLine { active, len: msg.chars().count() }
    }

    fn finish(self) {
        if self.active {
            eprint!("\r{}\r", " ".repeat(self.len));
            let _ = std::io::stderr().flush();
        }
    }
}
