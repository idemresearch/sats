//! The persisted watch-only wallet for the CLI. Chain access lives in
//! `crate::provider` — a `WalletCtx` is purely local state (sqlite-backed
//! BDK wallet), so opening one requires no chain configuration at all.

use anyhow::{Context, Result, bail};
use bdk_wallet::{PersistedWallet, Wallet};
use rusqlite::Connection;
use sats_core::bitcoin::Network;

use crate::config::network_name;
use crate::store::Store;

pub struct WalletCtx {
    pub wallet: PersistedWallet<Connection>,
    pub conn: Connection,
    pub network: Network,
    pub net_name: &'static str,
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
    store.create_private_dirs(dir)?;
    let mut conn = Connection::open(&db)?;
    Wallet::create(ext, int)
        .network(network)
        .create_wallet(&mut conn)
        .context("cannot create wallet")?;
    Ok(())
}

pub fn open(store: &Store, network: Network) -> Result<WalletCtx> {
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
    })
}

impl WalletCtx {
    pub fn persist(&mut self) -> Result<()> {
        self.wallet.persist(&mut self.conn)?;
        Ok(())
    }
}
