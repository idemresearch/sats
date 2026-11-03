//! Named wallets: one seed each, side by side under one data root.
//!
//! ```text
//! <data root>/wallets/<name>/seed.sealed
//! <data root>/wallets/<name>/<network>/…   (wallet db, grants, requests, relay link)
//! <config dir>/config.toml                 (shared: default wallet, network, providers)
//! ```
//!
//! A wallet and a network are independent choices: one seed serves every
//! test network. A surface picks the wallet (flag, then the configured
//! default, then the only wallet there is, then [`DEFAULT_WALLET`]) and
//! opens it as an ordinary [`Store`].

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::config::network_name;
use crate::store::{Store, harden_dir};

/// The name a first wallet gets when none is chosen.
pub const DEFAULT_WALLET: &str = "main";

pub const WALLET_NAME_RULE: &str =
    "a wallet name is 1-32 characters of a-z, 0-9, _ and -, starting with a letter or digit";

pub fn valid_wallet_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        && name.as_bytes()[0].is_ascii_alphanumeric()
}

/// One wallet in a listing.
pub struct WalletEntry {
    pub name: String,
    /// The networks this wallet has been initialized on.
    pub networks: Vec<&'static str>,
}

/// The set of named wallets under one data root.
pub struct Wallets {
    config_dir: PathBuf,
    data_root: PathBuf,
}

impl Wallets {
    pub fn new(config_dir: &Path, data_root: &Path) -> Wallets {
        Wallets {
            config_dir: config_dir.to_path_buf(),
            data_root: data_root.to_path_buf(),
        }
    }

    /// The wallet `name`, which may not exist yet.
    pub fn store(&self, name: &str) -> Result<Store> {
        if !valid_wallet_name(name) {
            bail!("invalid wallet name {name:?}: {WALLET_NAME_RULE}");
        }
        Ok(Store::named(&self.config_dir, &self.data_root, name))
    }

    /// A store for reading the shared configuration only.
    pub fn config_store(&self) -> Store {
        Store::named(&self.config_dir, &self.data_root, DEFAULT_WALLET)
    }

    pub fn exists(&self, name: &str) -> bool {
        valid_wallet_name(name) && self.wallets_dir().join(name).join("seed.sealed").exists()
    }

    fn wallets_dir(&self) -> PathBuf {
        self.data_root.join("wallets")
    }

    /// Wallets that hold a seed, by name.
    pub fn list(&self) -> Result<Vec<WalletEntry>> {
        let entries = match std::fs::read_dir(self.wallets_dir()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut wallets = Vec::new();
        for entry in entries {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if !self.exists(&name) {
                continue;
            }
            let store = self.store(&name)?;
            let networks = crate::config::NETWORKS
                .iter()
                .map(|network| network_name(*network))
                .filter(|net| store.wallet_db_path(net).exists())
                .collect();
            wallets.push(WalletEntry { name, networks });
        }
        wallets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(wallets)
    }

    /// Keep the wallets directory's listing private: it names every wallet.
    pub fn harden(&self) -> Result<()> {
        std::fs::create_dir_all(self.wallets_dir())?;
        harden_dir(&self.data_root)?;
        harden_dir(&self.wallets_dir())
    }

    /// Which wallet to use: the flag, then the configured default, then
    /// the only wallet there is, then [`DEFAULT_WALLET`].
    pub fn select(&self, flag: Option<&str>, configured: Option<&str>) -> Result<String> {
        if let Some(name) = flag.or(configured) {
            return Ok(name.to_string());
        }
        let mut existing = self.list()?;
        Ok(if existing.len() == 1 {
            existing.remove(0).name
        } else {
            DEFAULT_WALLET.to_string()
        })
    }

    /// A data root from before named wallets: a seed at its top level.
    /// It is refused with the move that adopts it as a named wallet, never
    /// moved silently.
    pub fn legacy_layout_error(&self) -> Option<String> {
        let seed = self.data_root.join("seed.sealed");
        if !seed.exists() {
            return None;
        }
        let root = self.data_root.display();
        let mut moves = vec![format!("\"{root}/seed.sealed\"")];
        for network in crate::config::NETWORKS {
            let net = network_name(network);
            if self.data_root.join(net).exists() {
                moves.push(format!("\"{root}/{net}\""));
            }
        }
        Some(format!(
            "this data directory predates named wallets: its seed is at {}.\n\
             Make it the wallet \"{DEFAULT_WALLET}\" (nothing is deleted):\n  \
             mkdir -p \"{root}/wallets/{DEFAULT_WALLET}\" && mv {} \"{root}/wallets/{DEFAULT_WALLET}/\"",
            seed.display(),
            moves.join(" ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallet_names_are_safe_path_components() {
        for good in ["main", "test", "a", "w-2", "cold_1", &"a".repeat(32)] {
            assert!(valid_wallet_name(good), "{good}");
        }
        for bad in [
            "",
            "-x",
            "_x",
            "Main",
            "a/b",
            "..",
            ".",
            "a b",
            &"a".repeat(33),
        ] {
            assert!(!valid_wallet_name(bad), "{bad}");
        }
    }

    #[test]
    fn selection_prefers_flag_then_default_then_the_only_wallet() {
        let dir = tempfile::TempDir::new().unwrap();
        let wallets = Wallets::new(dir.path(), dir.path());
        assert_eq!(wallets.select(None, None).unwrap(), DEFAULT_WALLET);
        let store = wallets.store("solo").unwrap();
        std::fs::create_dir_all(store.seed_path().parent().unwrap()).unwrap();
        std::fs::write(store.seed_path(), b"x").unwrap();
        assert_eq!(wallets.select(None, None).unwrap(), "solo");
        assert_eq!(wallets.select(None, Some("cfg")).unwrap(), "cfg");
        assert_eq!(wallets.select(Some("flag"), Some("cfg")).unwrap(), "flag");
        assert_eq!(store.wallet_name(), Some("solo"));
        assert_eq!(store.data_root(), dir.path());
        assert!(
            store
                .seed_path()
                .starts_with(dir.path().join("wallets/solo"))
        );
        assert!(wallets.store("../x").is_err());
    }

    #[test]
    fn a_legacy_layout_is_refused_with_the_move_that_adopts_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let wallets = Wallets::new(dir.path(), dir.path());
        assert!(wallets.legacy_layout_error().is_none());
        std::fs::write(dir.path().join("seed.sealed"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("signet")).unwrap();
        let message = wallets.legacy_layout_error().unwrap();
        assert!(message.contains("wallets/main"), "{message}");
        assert!(message.contains("/signet\""), "{message}");
        assert!(!message.contains("/testnet4"), "{message}");
    }
}
