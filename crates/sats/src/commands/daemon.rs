//! `sats daemon` — run and control the local signing daemon.
//!
//! satsd holds the master seed in memory so agent grants do not have to
//! hold it on disk. It signs only what a grant authorizes, and only while
//! a human has unlocked it.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use sats_core::bitcoin::Network;

use crate::config::network_name;
use crate::daemon::{self, Client};
use crate::store::Store;
use crate::{password, ui};

/// How long `start` waits for the spawned daemon to answer before
/// reporting failure. Binding a unix socket is fast; this is slack.
const START_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run(store: &Store, network: Network, auto_lock: &str) -> Result<()> {
    let auto_lock = parse_duration(auto_lock)?;
    let store = Store::open(store.dir_override())?;
    daemon::run(store, network, auto_lock)
}

/// Spawn `sats daemon run` in the background and wait for it to answer.
///
/// For anything long-lived, prefer `sats daemon run` under a supervisor
/// (a systemd user unit or a launchd agent): a detached child dies with
/// its session, and only a supervisor will bring it back.
pub fn start(store: &Store, network: Network, auto_lock: &str, json: bool) -> Result<()> {
    let net_name = network_name(network);
    parse_duration(auto_lock)?;

    if let Ok(mut client) = Client::open(store, net_name) {
        let status = client.status()?;
        bail!(
            "satsd is already running for {net_name} ({})",
            if status.locked { "locked" } else { "unlocked" }
        );
    }

    // A detached child must not hold the caller's stderr open — that
    // leaves the shell waiting on a process which never exits — so
    // diagnostics go to a log file rather than nowhere.
    let log_path = store.daemon_log_path(net_name);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("cannot open {}", log_path.display()))?;
    crate::store::harden_file(&log_path)?;

    let exe = std::env::current_exe().context("cannot locate the sats binary")?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("--network")
        .arg(net_name)
        .arg("daemon")
        .arg("run")
        .arg("--auto-lock")
        .arg(auto_lock)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log));
    if let Some(dir) = store.dir_override() {
        command.arg("--dir").arg(dir);
    }
    let mut child = command.spawn().context("cannot start satsd")?;

    // Poll rather than sleep a fixed time: a ready daemon answers at once.
    let deadline = std::time::Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(mut client) = Client::open(store, net_name)
            && client.status().is_ok()
        {
            break;
        }
        if let Ok(Some(exited)) = child.try_wait() {
            bail!(
                "satsd exited immediately ({exited}) — see {}",
                log_path.display()
            );
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            bail!(
                "satsd did not become ready within {START_TIMEOUT:?} — see {}",
                log_path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "running": true,
                "locked": true,
                "network": net_name,
                "socket": store.socket_path(net_name).display().to_string(),
            })
        );
    } else {
        ui::ok(&format!("satsd running on {net_name}"));
        ui::dim(&format!("logs:  {}", log_path.display()));
        ui::dim("next:  sats daemon unlock");
    }
    Ok(())
}

pub fn status(store: &Store, network: Network, json: bool) -> Result<()> {
    let net_name = network_name(network);
    let Ok(mut client) = Client::open(store, net_name) else {
        if json {
            println!(
                "{}",
                serde_json::json!({ "running": false, "network": net_name })
            );
            return Ok(());
        }
        bail!("satsd is not running for {net_name} — start it with: sats daemon start");
    };
    let info = client.status()?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "running": true,
                "locked": info.locked,
                "network": info.network,
                "grants": info.grants,
                "locks_in": info.locks_in,
                "version": info.version,
                "protocol": info.protocol,
            })
        );
        return Ok(());
    }

    let mut rows = vec![
        ("Network", info.network.clone()),
        (
            "State",
            if info.locked {
                "locked — cannot sign".to_string()
            } else {
                "unlocked".to_string()
            },
        ),
        ("Grants", info.grants.to_string()),
    ];
    if let Some(secs) = info.locks_in {
        rows.push(("Auto-lock", ui::human_duration(secs)));
    }
    rows.push(("Version", info.version.clone()));
    ui::kv_rows(&rows);
    if info.locked {
        ui::dim("agent sends refuse until: sats daemon unlock");
    }
    Ok(())
}

pub fn unlock(store: &Store, network: Network, json: bool) -> Result<()> {
    let net_name = network_name(network);
    let mut client = Client::open(store, net_name)?;
    if !store.seed_exists() {
        bail!("no wallet on this machine — run: sats init");
    }
    let pw = password::get(false)?;
    client.unlock(&pw)?;
    report(
        json,
        net_name,
        "unlocked",
        "satsd unlocked — agent sends can sign",
    )
}

pub fn lock(store: &Store, network: Network, json: bool) -> Result<()> {
    let net_name = network_name(network);
    let mut client = Client::open(store, net_name)?;
    client.lock()?;
    report(
        json,
        net_name,
        "locked",
        "satsd locked — the seed is gone from memory",
    )
}

pub fn stop(store: &Store, network: Network, json: bool) -> Result<()> {
    let net_name = network_name(network);
    let mut client = Client::open(store, net_name)?;
    client.shutdown()?;
    report(json, net_name, "stopped", "satsd stopped")
}

fn report(json: bool, net_name: &str, state: &str, message: &str) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::json!({ "network": net_name, "state": state })
        );
    } else {
        ui::ok(message);
    }
    Ok(())
}

fn parse_duration(value: &str) -> Result<Duration> {
    let parsed = humantime::parse_duration(value)
        .with_context(|| format!("invalid --auto-lock {value:?} (try 8h, 30m)"))?;
    if parsed.is_zero() {
        bail!("--auto-lock must be a positive duration");
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_lock_durations_are_validated() {
        assert_eq!(parse_duration("8h").unwrap(), Duration::from_secs(28_800));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1_800));
        assert!(parse_duration("0s").is_err(), "zero would lock instantly");
        assert!(parse_duration("soon").is_err());
    }
}
