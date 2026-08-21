//! On-disk layout, atomic writes, and secret file permissions.
//!
//! Default layout is XDG (`~/.config/sats` + `~/.local/share/sats`); the
//! `SATS_DIR` env var or `--dir` flag relocates everything under one
//! directory. Data is namespaced per network so wallets never mix.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sats_core::authz::Grant;
use sats_core::plan::{
    LegacyPlan, LegacyPlanStatus, PsbtSession, TransactionRecord, TransactionStatus,
};
use sats_core::seal::SealedBlob;

/// AAD binding the master seed blob to its purpose.
pub const AAD_SEED: &[u8] = b"sats-seed-v1";

/// AAD binding a grant-wrapped seed to its network and agent.
pub fn grant_aad(network: &str, agent: &str) -> Vec<u8> {
    format!("sats-grant-v1:{network}:{agent}").into_bytes()
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct Store {
    config_dir: PathBuf,
    data_dir: PathBuf,
    #[cfg(feature = "mcp")]
    override_dir: Option<PathBuf>,
}

impl Store {
    pub fn open(dir_override: Option<&Path>) -> Result<Store> {
        let (config_dir, data_dir) = match dir_override {
            Some(dir) => (dir.to_path_buf(), dir.to_path_buf()),
            None => {
                let dirs = directories::ProjectDirs::from("sh", "sats", "sats")
                    .context("cannot determine home directory")?;
                (
                    dirs.config_dir().to_path_buf(),
                    dirs.data_dir().to_path_buf(),
                )
            }
        };
        Ok(Store {
            config_dir,
            data_dir,
            #[cfg(feature = "mcp")]
            override_dir: dir_override.map(Path::to_path_buf),
        })
    }

    /// The `--dir`/`SATS_DIR` override this store was opened with, if any —
    /// lets long-running components reconstruct an identical store.
    #[cfg(feature = "mcp")]
    pub fn dir_override(&self) -> Option<&Path> {
        self.override_dir.as_deref()
    }

    pub fn config_path(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn seed_path(&self) -> PathBuf {
        self.data_dir.join("seed.sealed")
    }

    pub fn wallet_db_path(&self, network: &str) -> PathBuf {
        self.data_dir.join(network).join("wallet.sqlite")
    }

    /// Pre-refactor plan storage, retained for backward-reading only.
    pub fn legacy_plans_dir(&self, network: &str) -> PathBuf {
        self.data_dir.join(network).join("plans")
    }

    pub fn psbt_sessions_dir(&self, network: &str) -> PathBuf {
        self.data_dir.join(network).join("psbts")
    }

    pub fn transactions_dir(&self, network: &str) -> PathBuf {
        self.data_dir.join(network).join("transactions")
    }

    pub fn grants_dir(&self, network: &str) -> PathBuf {
        self.data_dir.join(network).join("grants")
    }

    pub fn seed_exists(&self) -> bool {
        self.seed_path().exists()
    }

    /// Read-only legacy state: new code never writes PSBT sessions —
    /// `sats send --export-psbt` produces file artifacts instead.
    pub fn load_psbt_session(&self, network: &str, id: &str) -> Result<PsbtSession> {
        let path = self.psbt_sessions_dir(network).join(format!("{id}.json"));
        if !path.exists() {
            bail!("no PSBT session {id}");
        }
        let bytes = fs::read(&path)?;
        let session: PsbtSession = serde_json::from_slice(&bytes)
            .with_context(|| format!("corrupt PSBT session {}", path.display()))?;
        if session.network != network {
            bail!(
                "PSBT session network {} does not match {network}",
                session.network
            );
        }
        session.clone().into_prepared()?;
        Ok(session)
    }

    pub fn latest_psbt_session(&self, network: &str) -> Result<Option<PsbtSession>> {
        let dir = self.psbt_sessions_dir(network);
        if !dir.exists() {
            return Ok(None);
        }
        let mut newest: Option<PsbtSession> = None;
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(session) = serde_json::from_slice::<PsbtSession>(&bytes) else {
                continue;
            };
            if session.network != network {
                continue;
            }
            if newest
                .as_ref()
                .is_none_or(|n| session.created_at > n.created_at)
            {
                newest = Some(session);
            }
        }
        Ok(newest)
    }

    pub fn delete_psbt_session(&self, network: &str, id: &str) -> Result<()> {
        let path = self.psbt_sessions_dir(network).join(format!("{id}.json"));
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    pub fn save_transaction(&self, network: &str, record: &TransactionRecord) -> Result<()> {
        if record.network != network {
            bail!(
                "transaction network {} does not match {network}",
                record.network
            );
        }
        record.tx()?;
        let path = self
            .transactions_dir(network)
            .join(format!("{}.json", record.txid));
        write_atomic(&path, &serde_json::to_vec_pretty(record)?, true)
    }

    /// Resolve a transaction by full/prefix txid or by the explicit PSBT
    /// session id that produced it.
    pub fn load_transaction(&self, network: &str, id: &str) -> Result<TransactionRecord> {
        let dir = self.transactions_dir(network);
        let exact = dir.join(format!("{id}.json"));
        if exact.exists() {
            return read_transaction(&exact, network);
        }
        if !dir.exists() {
            bail!("no transaction {id}");
        }

        let mut matches = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(record) = read_transaction(&path, network) else {
                continue;
            };
            if record.txid.starts_with(id) || record.source_id.as_deref() == Some(id) {
                matches.push(record);
            }
        }
        match matches.len() {
            0 => bail!("no transaction {id}"),
            1 => Ok(matches.remove(0)),
            _ => bail!("transaction id {id} is ambiguous"),
        }
    }

    pub fn latest_transaction(
        &self,
        network: &str,
        status: TransactionStatus,
    ) -> Result<Option<TransactionRecord>> {
        let dir = self.transactions_dir(network);
        if !dir.exists() {
            return Ok(None);
        }
        let mut newest: Option<TransactionRecord> = None;
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(record) = read_transaction(&path, network) else {
                continue;
            };
            if record.status == status
                && newest
                    .as_ref()
                    .is_none_or(|n| record.created_at > n.created_at)
            {
                newest = Some(record);
            }
        }
        Ok(newest)
    }

    pub fn load_legacy_plan(&self, network: &str, id: &str) -> Result<LegacyPlan> {
        let path = self.legacy_plans_dir(network).join(format!("{id}.json"));
        if !path.exists() {
            bail!("no legacy plan {id}");
        }
        harden_path(&path)?;
        let bytes = fs::read(&path)?;
        let plan: LegacyPlan = serde_json::from_slice(&bytes)
            .with_context(|| format!("corrupt legacy plan {}", path.display()))?;
        if plan.network != network {
            bail!(
                "legacy plan network {} does not match {network}",
                plan.network
            );
        }
        Ok(plan)
    }

    pub fn latest_legacy_plan(
        &self,
        network: &str,
        status: LegacyPlanStatus,
    ) -> Result<Option<LegacyPlan>> {
        let dir = self.legacy_plans_dir(network);
        if !dir.exists() {
            return Ok(None);
        }
        let mut newest: Option<LegacyPlan> = None;
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(plan) = serde_json::from_slice::<LegacyPlan>(&bytes) else {
                continue;
            };
            if plan.network != network {
                continue;
            }
            if plan.status == status
                && newest
                    .as_ref()
                    .is_none_or(|n| plan.created_at > n.created_at)
            {
                harden_path(&path)?;
                newest = Some(plan);
            }
        }
        Ok(newest)
    }

    pub fn delete_legacy_plan(&self, network: &str, id: &str) -> Result<()> {
        let path = self.legacy_plans_dir(network).join(format!("{id}.json"));
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    pub fn save_grant(&self, network: &str, grant: &Grant) -> Result<()> {
        let path = self
            .grants_dir(network)
            .join(format!("{}.json", grant.agent));
        write_atomic(&path, &serde_json::to_vec_pretty(grant)?, true)
    }

    pub fn load_grant(&self, network: &str, agent: &str) -> Result<Option<Grant>> {
        let path = self.grants_dir(network).join(format!("{agent}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        let grant = serde_json::from_slice(&bytes)
            .with_context(|| format!("corrupt grant {}", path.display()))?;
        Ok(Some(grant))
    }

    /// Returns whether a grant existed. Deletion is revocation: no key
    /// material survives it.
    pub fn delete_grant(&self, network: &str, agent: &str) -> Result<bool> {
        let path = self.grants_dir(network).join(format!("{agent}.json"));
        if !path.exists() {
            return Ok(false);
        }
        fs::remove_file(&path)?;
        Ok(true)
    }

    /// All grants for a network, deleting expired ones as they're found.
    pub fn active_grants(&self, network: &str, now_unix: u64) -> Result<Vec<Grant>> {
        let dir = self.grants_dir(network);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut grants = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(grant) = serde_json::from_slice::<Grant>(&bytes) else {
                continue;
            };
            if grant.is_expired(now_unix) {
                let _ = fs::remove_file(&path);
                continue;
            }
            grants.push(grant);
        }
        grants.sort_by(|a, b| a.agent.cmp(&b.agent));
        Ok(grants)
    }

    pub fn read_seed(&self) -> Result<SealedBlob> {
        let path = self.seed_path();
        if !path.exists() {
            bail!("no wallet — run: sats init");
        }
        let bytes = fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("corrupt seed file {}", path.display()))
    }

    pub fn write_seed(&self, blob: &SealedBlob) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(blob)?;
        write_atomic(&self.seed_path(), &bytes, true)
    }
}

fn read_transaction(path: &Path, network: &str) -> Result<TransactionRecord> {
    let bytes = fs::read(path)?;
    let record: TransactionRecord = serde_json::from_slice(&bytes)
        .with_context(|| format!("corrupt transaction {}", path.display()))?;
    if record.network != network {
        bail!(
            "transaction network {} does not match {network}",
            record.network
        );
    }
    record.tx()?;
    Ok(record)
}

#[cfg(unix)]
fn harden_path(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn harden_path(_path: &Path) -> Result<()> {
    Ok(())
}

/// Write via tmp file + fsync + rename so a crash never leaves a torn file.
/// `secret` restricts the file to owner read/write before any bytes land.
pub fn write_atomic(path: &Path, bytes: &[u8], secret: bool) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    {
        let mut file =
            fs::File::create(&tmp).with_context(|| format!("cannot write {}", tmp.display()))?;
        set_secret_perms(&file, secret)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn set_secret_perms(file: &fs::File, secret: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if secret {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_secret_perms(_file: &fs::File, _secret: bool) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use sats_core::bitcoin::{
        Amount, OutPoint, Psbt, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
        absolute::LockTime, transaction::Version,
    };
    use sats_core::plan::{PreparedSpend, TransactionRecord, TransactionStatus};
    use tempfile::TempDir;

    use super::*;

    fn test_transaction() -> Transaction {
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ScriptBuf::new(),
            }],
        }
    }

    fn record(source_id: Option<String>) -> TransactionRecord {
        TransactionRecord::from_transaction(
            "signet".into(),
            "tb1ptest".into(),
            1_000,
            100,
            42,
            0,
            source_id,
            &test_transaction(),
        )
    }

    #[test]
    fn finalized_transactions_are_private_and_resolvable() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        let record = record(Some("session-id".into()));

        store.save_transaction("signet", &record).unwrap();
        let path = store
            .transactions_dir("signet")
            .join(format!("{}.json", record.txid));
        let json = fs::read_to_string(&path).unwrap();
        assert!(json.contains("tx_hex"));
        assert!(!json.contains("psbt"));
        assert_eq!(
            store.load_transaction("signet", "session-id").unwrap().txid,
            record.txid
        );
        assert_eq!(
            store
                .latest_transaction("signet", TransactionStatus::Pending)
                .unwrap()
                .unwrap()
                .txid,
            record.txid
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn explicit_psbt_sessions_are_private_and_legacy_plans_still_load() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        let tx = test_transaction();
        let psbt = Psbt::from_unsigned_tx(tx).unwrap();
        let prepared =
            PreparedSpend::new("signet".into(), "tb1ptest".into(), 1_000, 100, 42, 0, psbt);
        let session = prepared.session();

        // Sessions are read-only legacy state: fabricate one on disk the
        // way an older release would have written it.
        let session_path = store
            .psbt_sessions_dir("signet")
            .join(format!("{}.json", session.id));
        write_atomic(
            &session_path,
            &serde_json::to_vec_pretty(&session).unwrap(),
            true,
        )
        .unwrap();
        assert_eq!(
            store.load_psbt_session("signet", &session.id).unwrap().id,
            session.id
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&session_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let legacy_path = store.legacy_plans_dir("signet").join("legacy.json");
        let legacy = serde_json::json!({
            "id": "legacy",
            "network": "signet",
            "recipient": "tb1ptest",
            "amount_sat": 1000,
            "fee_sat": 100,
            "created_at": 42,
            "status": "unsigned",
            "psbt": session.psbt,
            "excluded_utxos": 0,
        });
        write_atomic(&legacy_path, &serde_json::to_vec(&legacy).unwrap(), false).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&legacy_path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let loaded = store.load_legacy_plan("signet", "legacy").unwrap();
        assert_eq!(loaded.id, "legacy");
        assert_eq!(loaded.into_prepared().unwrap().id, session.id);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(legacy_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
