//! On-disk layout, atomic writes, and secret file permissions.
//!
//! Default layout is XDG (`~/.config/sats` + `~/.local/share/sats`); the
//! `SATS_DIR` env var or `--dir` flag relocates everything under one
//! directory. Data is namespaced per network so wallets never mix.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sats_core::seal::SealedBlob;

/// AAD binding the master seed blob to its purpose.
pub const AAD_SEED: &[u8] = b"sats-seed-v1";

pub struct Store {
    config_dir: PathBuf,
    data_dir: PathBuf,
}

impl Store {
    pub fn open(dir_override: Option<&Path>) -> Result<Store> {
        let (config_dir, data_dir) = match dir_override {
            Some(dir) => (dir.to_path_buf(), dir.to_path_buf()),
            None => {
                let dirs = directories::ProjectDirs::from("sh", "sats", "sats")
                    .context("cannot determine home directory")?;
                (dirs.config_dir().to_path_buf(), dirs.data_dir().to_path_buf())
            }
        };
        Ok(Store { config_dir, data_dir })
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

    pub fn plans_dir(&self, network: &str) -> PathBuf {
        self.data_dir.join(network).join("plans")
    }

    pub fn grants_dir(&self, network: &str) -> PathBuf {
        self.data_dir.join(network).join("grants")
    }

    pub fn seed_exists(&self) -> bool {
        self.seed_path().exists()
    }

    pub fn read_seed(&self) -> Result<SealedBlob> {
        let path = self.seed_path();
        if !path.exists() {
            bail!("no wallet — run: sats init");
        }
        let bytes = fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("corrupt seed file {}", path.display()))
    }

    pub fn write_seed(&self, blob: &SealedBlob) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(blob)?;
        write_atomic(&self.seed_path(), &bytes, true)
    }
}

/// Write via tmp file + fsync + rename so a crash never leaves a torn file.
/// `secret` restricts the file to owner read/write before any bytes land.
pub fn write_atomic(path: &Path, bytes: &[u8], secret: bool) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    {
        let mut file = fs::File::create(&tmp).with_context(|| format!("cannot write {}", tmp.display()))?;
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
