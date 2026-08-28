//! Opt-in per-user launchd supervision. No keys or agent credentials live here.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sats_core::bitcoin::Network;
use sats_core::bitcoin::hashes::{Hash, sha256};
use serde_json::{Value, json};

use crate::config::network_name;
use crate::daemon::Client;
use crate::store::{Store, harden_file, write_atomic};
use crate::ui;

struct Service {
    label: String,
    path: PathBuf,
    domain: String,
    socket: PathBuf,
}

// Canonicalize existing ancestors as well as relative paths. The wallet/socket
// need not have been created yet, and /tmp is a symlink on macOS.
fn absolute(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path).context("cannot resolve service path");
    }
    let path = std::path::absolute(path)?;
    let parent = path.parent().context("service path has no parent")?;
    Ok(absolute(parent)?.join(path.file_name().context("service path has no name")?))
}

fn identity(network: &str, socket: &Path) -> String {
    let digest = sha256::Hash::hash(socket.as_os_str().as_encoded_bytes());
    format!("sh.sats.satsd.{network}.{}", &digest.to_string()[..16])
}

impl Service {
    fn new(store: &Store, network: Network) -> Result<Self> {
        let home = directories::BaseDirs::new().context("cannot determine home directory")?;
        let socket = absolute(&store.socket_path(network_name(network)))?;
        let label = identity(network_name(network), &socket);
        let uid = Command::new("/usr/bin/id").arg("-u").output()?;
        if !uid.status.success() {
            bail!("cannot determine current user for launchd");
        }
        let uid = std::str::from_utf8(&uid.stdout)?.trim().parse::<u32>()?;
        Ok(Self {
            path: home
                .home_dir()
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist")),
            label,
            domain: format!("gui/{uid}"),
            socket,
        })
    }

    fn target(&self) -> String {
        format!("{}/{}", self.domain, self.label)
    }

    fn management_lock(&self) -> Result<fs::File> {
        let parent = self.socket.parent().context("socket has no parent")?;
        fs::create_dir_all(parent)?;
        crate::store::harden_dir(parent)?;
        let path = self.socket.with_extension("service-lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)?;
        harden_file(&path)?;
        file.try_lock()
            .map_err(|_| anyhow::anyhow!("another service operation is in progress"))?;
        Ok(file)
    }

    /// None means not registered; Some(false) means registered but stopped.
    fn state(&self) -> Result<Option<bool>> {
        let output = Command::new("/bin/launchctl")
            .args(["print", &self.target()])
            .output()?;
        if output.status.success() {
            return Ok(Some(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.trim_start().starts_with("pid = ")),
            ));
        }
        let error = String::from_utf8_lossy(&output.stderr);
        if error.contains("Could not find service")
            || error.contains("Could not find specified service")
        {
            return Ok(None);
        }
        bail!(
            "cannot inspect launchd service (a logged-in macOS GUI session is required): {}",
            error.trim()
        )
    }

    fn launchctl(&self, args: &[&str]) -> Result<()> {
        let output = Command::new("/bin/launchctl").args(args).output()?;
        if !output.status.success() {
            bail!(
                "launchctl {} failed: {}",
                args[0],
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    fn read_job(&self) -> Result<Value> {
        let output = Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-", "--"])
            .arg(&self.path)
            .output()?;
        if !output.status.success() {
            bail!("cannot read installed service {}", self.path.display());
        }
        let job: Value = serde_json::from_slice(&output.stdout)?;
        if job["Label"] != self.label {
            bail!("installed service identity does not match this wallet/network");
        }
        Ok(job)
    }

    fn start(&self, store: &Store, network: Network) -> Result<()> {
        match self.state()? {
            Some(true) => {}
            Some(false) => self.launchctl(&["kickstart", &self.target()])?,
            None => self.launchctl(&[
                "bootstrap",
                &self.domain,
                self.path.to_str().context("non-UTF8 service path")?,
            ])?,
        }
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if Client::probe(store, network_name(network)).is_ok() {
                return Ok(());
            }
            if Instant::now() >= until {
                bail!(
                    "managed satsd did not become ready; see {}",
                    store.daemon_log_path(network_name(network)).display()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn job(
    service: &Service,
    store: &Store,
    network: Network,
    auto_lock: &str,
    exe: &Path,
) -> Result<Value> {
    let mut args = vec![
        exe.to_str().context("non-UTF8 binary path")?.to_owned(),
        "--network".into(),
        network_name(network).into(),
    ];
    if let Some(dir) = store.dir_override() {
        args.extend([
            "--dir".into(),
            absolute(dir)?
                .to_str()
                .context("non-UTF8 wallet path")?
                .into(),
        ]);
    }
    args.extend([
        "daemon".into(),
        "run".into(),
        "--auto-lock".into(),
        humantime::format_duration(super::parse_duration(auto_lock)?).to_string(),
    ]);
    let mut env = serde_json::Map::new();
    // Freeze only the public location settings used by Store. Never copy the
    // calling environment (which can contain passwords or capability tokens).
    if store.dir_override().is_none() {
        let home = directories::BaseDirs::new().context("cannot determine home directory")?;
        env.insert("HOME".into(), json!(absolute(home.home_dir())?));
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
            env.insert(
                "XDG_RUNTIME_DIR".into(),
                json!(absolute(Path::new(&runtime))?),
            );
        }
    }
    Ok(json!({
        "Label": service.label,
        "ProgramArguments": args,
        "EnvironmentVariables": env,
        "RunAtLoad": true,
        "KeepAlive": { "SuccessfulExit": false },
        "ThrottleInterval": 10,
        "Umask": 63,
        "StandardOutPath": absolute(&store.daemon_log_path(network_name(network)))?,
        "StandardErrorPath": absolute(&store.daemon_log_path(network_name(network)))?,
    }))
}

fn xml(job: &Value) -> Result<Vec<u8>> {
    // plutil encodes argument strings correctly, including XML metacharacters.
    // No shell interpolation or hand-built XML is involved.
    let mut child = Command::new("/usr/bin/plutil")
        .args(["-convert", "xml1", "-o", "-", "--", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .context("plutil stdin unavailable")?
        .write_all(&serde_json::to_vec(job)?)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("cannot encode launchd property list");
    }
    Ok(output.stdout)
}

fn report(service: &Service, network: Network, state: &str, json_output: bool) {
    if json_output {
        println!(
            "{}",
            json!({"network": network_name(network), "state": state,
            "service": service.label, "plist": service.path})
        );
    } else {
        ui::ok(&format!(
            "satsd service {state} for {}",
            network_name(network)
        ));
        ui::dim(&format!("service: {}", service.path.display()));
    }
}

pub fn install(store: &Store, network: Network, auto_lock: &str, json_output: bool) -> Result<()> {
    let service = Service::new(store, network)?;
    let _lock = service.management_lock()?;
    let desired = job(
        &service,
        store,
        network,
        auto_lock,
        &std::env::current_exe()?,
    )?;
    let existing = service
        .path
        .exists()
        .then(|| service.read_job())
        .transpose()?;
    let state = service.state()?;
    if state != Some(true) && Client::open(store, network_name(network)).is_ok() {
        bail!(
            "an unmanaged satsd is running; stop it explicitly with sats daemon stop before installing"
        );
    }
    if existing.as_ref() != Some(&desired) {
        if state == Some(true) {
            bail!(
                "service configuration changed while satsd is running; run sats daemon stop before reinstalling"
            );
        }
        if state.is_some() {
            service.launchctl(&["bootout", &service.target()])?;
        }
        let log = store.daemon_log_path(network_name(network));
        fs::create_dir_all(log.parent().context("log has no parent")?)?;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&log)?;
        harden_file(&log)?;
        write_atomic(&service.path, &xml(&desired)?, true)?;
    }
    service.start(store, network)?;
    report(&service, network, "installed", json_output);
    if !json_output {
        ui::dim("starts locked at login and after a crash; unlock with: sats daemon unlock");
    }
    Ok(())
}

pub fn start_installed(
    store: &Store,
    network: Network,
    auto_lock: Option<&str>,
    json_output: bool,
) -> Result<bool> {
    let service = Service::new(store, network)?;
    if !service.path.exists() {
        return Ok(false);
    }
    let _lock = service.management_lock()?;
    let installed = service.read_job()?;
    if let Some(requested) = auto_lock {
        let args = installed["ProgramArguments"]
            .as_array()
            .context("invalid installed service arguments")?;
        let duration = args
            .windows(2)
            .find(|pair| pair[0] == "--auto-lock")
            .and_then(|pair| pair[1].as_str())
            .context("installed service has no auto-lock duration")?;
        if super::parse_duration(requested)? != super::parse_duration(duration)? {
            bail!(
                "--auto-lock conflicts with the installed service; stop satsd and run sats daemon install --auto-lock {requested}"
            );
        }
    }
    if service.state()? != Some(true) && Client::open(store, network_name(network)).is_ok() {
        bail!(
            "an unmanaged satsd is running; stop it explicitly before starting the installed service"
        );
    }
    service.start(store, network)?;
    let status = Client::probe(store, network_name(network))?;
    if json_output {
        println!(
            "{}",
            json!({"running": true, "locked": status.locked,
            "network": network_name(network), "socket": store.socket_path(network_name(network))})
        );
    } else {
        ui::ok(&format!(
            "managed satsd running on {}",
            network_name(network)
        ));
    }
    Ok(true)
}

pub fn uninstall(store: &Store, network: Network, json_output: bool) -> Result<()> {
    let service = Service::new(store, network)?;
    let _lock = service.management_lock()?;
    if service.path.exists() {
        service.read_job()?;
        if service.state()?.is_some() {
            service.launchctl(&["bootout", &service.target()])?;
        }
        fs::remove_file(&service.path)?;
    }
    report(&service, network, "uninstalled", json_output);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_identity_and_arguments_are_isolated_and_private() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("wallet & <test> 'quoted'");
        fs::create_dir(&dir).unwrap();
        let store = Store::open(Some(&dir)).unwrap();
        let socket = absolute(&store.socket_path("signet")).unwrap();
        let label = identity("signet", &socket);
        assert_ne!(label, identity("mainnet", &socket));
        assert_ne!(label, identity("signet", &temp.path().join("other/d.sock")));
        let service = Service {
            label,
            socket,
            path: temp.path().join("service.plist"),
            domain: "gui/123".into(),
        };
        let value = job(
            &service,
            &store,
            Network::Signet,
            "8h",
            Path::new("/test/sats & <binary>"),
        )
        .unwrap();
        let encoded = xml(&value).unwrap();
        write_atomic(&service.path, &encoded, true).unwrap();
        assert_eq!(service.read_job().unwrap(), value);
        assert_eq!(
            value["ProgramArguments"][4],
            absolute(&dir).unwrap().to_str().unwrap()
        );
        assert_eq!(value["EnvironmentVariables"], json!({}));
        assert_eq!(value["KeepAlive"]["SuccessfulExit"], false);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&service.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let held = service.management_lock().unwrap();
        assert!(service.management_lock().is_err());
        drop(held);
        assert!(service.management_lock().is_ok());
    }
}
