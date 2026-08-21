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
use sats_core::plan::{Plan, PlanStatus};
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

    pub fn plans_dir(&self, network: &str) -> PathBuf {
        self.data_dir.join(network).join("plans")
    }

    pub fn grants_dir(&self, network: &str) -> PathBuf {
        self.data_dir.join(network).join("grants")
    }

    pub fn seed_exists(&self) -> bool {
        self.seed_path().exists()
    }

    pub fn save_plan(&self, network: &str, plan: &Plan) -> Result<()> {
        let path = self.plans_dir(network).join(format!("{}.json", plan.id));
        write_atomic(&path, &serde_json::to_vec_pretty(plan)?, false)
    }

    pub fn load_plan(&self, network: &str, id: &str) -> Result<Plan> {
        let path = self.plans_dir(network).join(format!("{id}.json"));
        if !path.exists() {
            bail!("no plan {id}");
        }
        let bytes = fs::read(&path)?;
        serde_json::from_slice(&bytes).with_context(|| format!("corrupt plan {}", path.display()))
    }

    /// The newest plan in the given status, if any.
    pub fn latest_plan(&self, network: &str, status: PlanStatus) -> Result<Option<Plan>> {
        let dir = self.plans_dir(network);
        if !dir.exists() {
            return Ok(None);
        }
        let mut newest: Option<Plan> = None;
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(plan) = serde_json::from_slice::<Plan>(&bytes) else {
                continue;
            };
            if plan.status == status
                && newest
                    .as_ref()
                    .is_none_or(|n| plan.created_at > n.created_at)
            {
                newest = Some(plan);
            }
        }
        Ok(newest)
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
