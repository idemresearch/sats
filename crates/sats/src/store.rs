//! On-disk layout, atomic writes, and secret file permissions.
//!
//! Default layout is XDG (`~/.config/sats` + `~/.local/share/sats`); the
//! `SATS_DIR` env var or `--dir` flag relocates everything under one
//! directory. Data is namespaced per network so wallets never mix.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sats_core::authz::{AGENT_NAME_RULE, Grant, valid_agent_name};
use sats_core::event::AgentEvent;
use sats_core::plan::{LegacyPlan, PsbtSession, TransactionRecord};
use sats_core::request::AgentRequest;
use sats_core::seal::SealedBlob;

/// AAD binding the master seed blob to its purpose.
pub const AAD_SEED: &[u8] = b"sats-seed-v1";

/// The clock for authorization decisions. Fails closed: a broken system
/// clock refuses to authorize rather than reporting 1970, which would
/// un-expire every grant and approval.
pub fn now_checked() -> Result<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .context("system clock is before the unix epoch — refusing to authorize")
}

/// One line of the append-only event log, read tolerantly. `Unknown`
/// carries a syntactically valid line whose event kind or format version
/// this build cannot interpret — written by a newer sats — so audit
/// views can show it raw instead of hiding it.
#[derive(Debug, Clone)]
pub enum EventLine {
    Event(AgentEvent),
    Unknown(serde_json::Value),
}

/// Record timestamps only (`created_at`, `resolved_at`, journal lines).
/// Never use this for an authorization decision — expiry checked against
/// its 0 fallback fails open. Decisions take [`now_checked`].
pub fn unix_now() -> u64 {
    now_checked().unwrap_or(0)
}

/// Exclusive hold on a network's grant state; released on drop.
pub struct GrantLock {
    _file: fs::File,
}

/// Exclusive hold on one agent request's execution; released on drop.
/// The lock lives in a sibling `.lock` file, never the record itself:
/// `write_atomic` renames over the record path, which would silently
/// detach a lock held on it.
pub struct RequestClaim {
    _file: fs::File,
}

pub struct Store {
    config_dir: PathBuf,
    data_dir: PathBuf,
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
            override_dir: dir_override.map(Path::to_path_buf),
        })
    }

    /// The `--dir`/`SATS_DIR` override this store was opened with, if any —
    /// lets long-running components reconstruct an identical store.
    pub fn dir_override(&self) -> Option<&Path> {
        self.override_dir.as_deref()
    }

    /// Create `dir` and restrict it — and every component from the data
    /// directory down — to the owner. Files inside are already 0600;
    /// this keeps the directory *listings* (agent names, txids, request
    /// ids) private too. Re-applies 0700 to components that already
    /// exist, so older installations harden lazily as they are touched.
    pub fn create_private_dirs(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let mut current = dir;
        loop {
            if current.starts_with(&self.data_dir) {
                harden_dir(current)?;
            }
            if current == self.data_dir {
                break;
            }
            match current.parent() {
                Some(parent) if parent.starts_with(&self.data_dir) => current = parent,
                _ => break,
            }
        }
        Ok(())
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

    pub fn agent_requests_dir(&self, network: &str) -> PathBuf {
        self.data_dir.join(network).join("agent-requests")
    }

    /// The satsd socket for a network.
    ///
    /// Prefers the XDG runtime directory, which is per-user, mode 0700,
    /// and cleared at logout — the right home for a live socket. An
    /// explicit `--dir`/`SATS_DIR` keeps everything under that directory
    /// instead, so isolated runs and tests never collide.
    pub fn socket_path(&self, network: &str) -> PathBuf {
        if self.override_dir.is_none()
            && let Some(run) = std::env::var_os("XDG_RUNTIME_DIR")
            && !run.is_empty()
        {
            return PathBuf::from(run)
                .join("sats")
                .join(format!("{network}.sock"));
        }
        // Kept short: unix socket paths have a low length limit.
        self.data_dir.join(network).join("d.sock")
    }

    /// Where a backgrounded daemon's diagnostics go. A detached process
    /// has no terminal to write to, and silently discarding the reason it
    /// failed to start would be worse than a file.
    pub fn daemon_log_path(&self, network: &str) -> PathBuf {
        self.data_dir.join(network).join("satsd.log")
    }

    pub fn events_dir(&self, network: &str) -> PathBuf {
        self.data_dir.join(network).join("events")
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

    /// Every transaction record for a network, newest first. An unreadable
    /// file is skipped with a warning rather than failing the listing.
    pub fn list_transactions(&self, network: &str) -> Result<Vec<TransactionRecord>> {
        let dir = self.transactions_dir(network);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            match read_transaction(&path, network) {
                Ok(record) => records.push(record),
                Err(_) => {
                    eprintln!(
                        "⚠ skipping unreadable transaction record {}",
                        path.display()
                    );
                }
            }
        }
        records.sort_by_key(|record| std::cmp::Reverse(record.created_at));
        Ok(records)
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

    pub fn delete_legacy_plan(&self, network: &str, id: &str) -> Result<()> {
        let path = self.legacy_plans_dir(network).join(format!("{id}.json"));
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// Take the per-network advisory grant lock. Blocks until any
    /// concurrent holder releases it. Grant budgets are read-modify-write
    /// state: every reserve, refund, replacement, or revocation must happen
    /// under this lock so concurrent sends cannot double-draw a budget and
    /// a revoked grant cannot be resurrected by an in-flight save. Agent
    /// request rewrites — outcomes and approvals — share it, so budget and
    /// approval state serialize as one history. The underlying flock-style
    /// lock contends between separate opens even within one process, so it
    /// also serializes sends inside one server.
    pub fn lock_grants(&self, network: &str) -> Result<GrantLock> {
        let dir = self.grants_dir(network);
        self.create_private_dirs(&dir)?;
        let path = dir.join(".lock");
        let file = fs::File::create(&path)
            .with_context(|| format!("cannot open grant lock {}", path.display()))?;
        file.lock()
            .with_context(|| format!("cannot lock {}", path.display()))?;
        Ok(GrantLock { _file: file })
    }

    pub fn save_grant(&self, network: &str, grant: &Grant) -> Result<()> {
        let path = self
            .grants_dir(network)
            .join(format!("{}.json", agent_component(&grant.agent)?));
        write_atomic(&path, &serde_json::to_vec_pretty(grant)?, true)
    }

    /// Load a grant, refusing the v1 format outright.
    ///
    /// A v1 record stored the master seed re-sealed beside its own key,
    /// so honoring one would preserve exactly the weakness the daemon
    /// removes. It is read well enough to name itself and is then
    /// refused with the commands that replace it.
    ///
    /// The record's own `network` is checked against the requested one,
    /// like every sibling loader. A v1 grant sealed its seed under an AAD
    /// naming the network, so a grant copied across networks failed to
    /// unseal and was inert; a v2 grant carries no seal to bind it, so the
    /// check is what keeps a signet grant from authorizing a mainnet
    /// signature when its file is dropped into another network's directory.
    pub fn load_grant(&self, network: &str, agent: &str) -> Result<Option<Grant>> {
        let agent = agent_component(agent)?;
        let path = self.grants_dir(network).join(format!("{agent}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("corrupt grant {}", path.display()))?;
        if is_legacy_grant(&value) {
            bail!("{}", legacy_grant_message(agent, &path));
        }
        let grant: Grant = serde_json::from_value(value)
            .with_context(|| format!("corrupt grant {}", path.display()))?;
        // Fail closed on records from the future: a newer sats may have
        // written restrictive fields this build cannot see, and ignoring
        // them would widen the agent's authority, never narrow it.
        if grant.format_version > sats_core::authz::GRANT_FORMAT_VERSION {
            bail!(
                "grant for {agent:?} was written by a newer sats (format v{}, this build reads \
                 up to v{}) — upgrade sats, or re-issue it with: sats agent grant {agent} \
                 --budget <sats>",
                grant.format_version,
                sats_core::authz::GRANT_FORMAT_VERSION,
            );
        }
        if grant.network != network {
            bail!(
                "grant network {} does not match {network} — a grant authorizes only the \
                 network it was created for; re-issue it with: sats agent grant {agent} \
                 --budget <sats> --network {network}",
                grant.network
            );
        }
        Ok(Some(grant))
    }

    /// Agent names whose grant files are still in the v1 format, so read
    /// surfaces can report them instead of silently showing no grant.
    pub fn legacy_grants(&self, network: &str) -> Result<Vec<String>> {
        let dir = self.grants_dir(network);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut agents = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            if is_legacy_grant(&value) {
                agents.push(
                    value["agent"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            path.file_stem()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned()
                        }),
                );
            }
        }
        agents.sort();
        Ok(agents)
    }

    /// Returns whether a grant existed. Deletion is revocation: no key
    /// material survives it.
    pub fn delete_grant(&self, network: &str, agent: &str) -> Result<bool> {
        let agent = agent_component(agent)?;
        let path = self.grants_dir(network).join(format!("{agent}.json"));
        if !path.exists() {
            return Ok(false);
        }
        fs::remove_file(&path)?;
        Ok(true)
    }

    /// All unexpired grants for a network. Purely a read: expired files
    /// are skipped, never deleted, so unauthenticated surfaces (the
    /// daemon's status op) can call this without mutating grant state.
    /// [`Store::prune_expired_grants`] is the explicit cleanup.
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
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            // Legacy records are reported by `legacy_grants`, never
            // treated as authority here.
            if is_legacy_grant(&value) {
                continue;
            }
            let Ok(grant) = serde_json::from_value::<Grant>(value) else {
                continue;
            };
            if grant.is_expired(now_unix) {
                continue;
            }
            grants.push(grant);
        }
        grants.sort_by(|a, b| a.agent.cmp(&b.agent));
        Ok(grants)
    }

    /// Delete expired grant files, returning the agent names pruned.
    /// Takes the grant lock so a prune never races an in-flight send's
    /// budget write or a grant replacement.
    pub fn prune_expired_grants(&self, network: &str, now_unix: u64) -> Result<Vec<String>> {
        let dir = self.grants_dir(network);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let _lock = self.lock_grants(network)?;
        let mut pruned = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            if is_legacy_grant(&value) {
                continue;
            }
            let Ok(grant) = serde_json::from_value::<Grant>(value) else {
                continue;
            };
            if grant.is_expired(now_unix) && fs::remove_file(&path).is_ok() {
                pruned.push(grant.agent);
            }
        }
        pruned.sort();
        Ok(pruned)
    }

    /// Atomically create an agent request record and take its execution
    /// lock. `Ok(None)` means a record with this id already exists — the
    /// idempotency-hit signal; nothing is written in that case.
    pub fn create_agent_request(
        &self,
        network: &str,
        request: &AgentRequest,
    ) -> Result<Option<RequestClaim>> {
        // Both components are gated before anything touches the disk.
        let file = format!("{}.json", request_id_component(&request.id)?);
        let dir = self
            .agent_requests_dir(network)
            .join(agent_component(&request.agent)?);
        self.create_private_dirs(&dir)?;
        let path = dir.join(file);
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
            Err(err) => {
                return Err(err).with_context(|| format!("cannot create {}", path.display()));
            }
        };
        // Owner-only before any bytes land: requests carry payment metadata.
        set_secret_perms(&file, true)?;
        file.write_all(&serde_json::to_vec_pretty(request)?)?;
        file.sync_all()?;
        let claim = self
            .claim_agent_request(network, &request.agent, &request.id)?
            .context("freshly created request is already executing")?;
        Ok(Some(claim))
    }

    /// Take an existing request's execution lock without blocking.
    /// `Ok(None)` means another execution holds it right now.
    pub fn claim_agent_request(
        &self,
        network: &str,
        agent: &str,
        id: &str,
    ) -> Result<Option<RequestClaim>> {
        let file = format!("{}.lock", request_id_component(id)?);
        let dir = self
            .agent_requests_dir(network)
            .join(agent_component(agent)?);
        self.create_private_dirs(&dir)?;
        let path = dir.join(file);
        let file = fs::File::create(&path)
            .with_context(|| format!("cannot open request lock {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(RequestClaim { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(err)) => {
                Err(err).with_context(|| format!("cannot lock {}", path.display()))
            }
        }
    }

    pub fn load_agent_request(
        &self,
        network: &str,
        agent: &str,
        id: &str,
    ) -> Result<Option<AgentRequest>> {
        let path = self
            .agent_requests_dir(network)
            .join(agent_component(agent)?)
            .join(format!("{}.json", request_id_component(id)?));
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        let request: AgentRequest = serde_json::from_slice(&bytes)
            .with_context(|| format!("corrupt agent request {}", path.display()))?;
        Ok(Some(request))
    }

    /// Rewrite a request record. Callers must hold the grant lock: request
    /// rewrites share it with grant writes so approvals, outcomes, and
    /// budget decisions serialize as one history.
    pub fn save_agent_request(&self, network: &str, request: &AgentRequest) -> Result<()> {
        let path = self
            .agent_requests_dir(network)
            .join(agent_component(&request.agent)?)
            .join(format!("{}.json", request_id_component(&request.id)?));
        write_atomic(&path, &serde_json::to_vec_pretty(request)?, true)
    }

    /// Every agent request for a network, newest first, across agents.
    /// An unreadable or unsupported file is skipped with a warning rather
    /// than failing the listing.
    pub fn list_agent_requests(&self, network: &str) -> Result<Vec<AgentRequest>> {
        let root = self.agent_requests_dir(network);
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut requests = Vec::new();
        for agent_entry in fs::read_dir(&root)? {
            let agent_dir = agent_entry?.path();
            if !agent_dir.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&agent_dir)? {
                let path = entry?.path();
                if path.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                let readable = fs::read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<AgentRequest>(&bytes).ok());
                match readable {
                    Some(request) if request.version_supported() => requests.push(request),
                    _ => eprintln!("⚠ skipping unreadable agent request {}", path.display()),
                }
            }
        }
        requests.sort_by_key(|request| std::cmp::Reverse(request.created_at));
        Ok(requests)
    }

    /// Resolve one agent request by exact id or unique prefix, across all
    /// agents. Ambiguity is an explicit error.
    pub fn find_agent_request(&self, network: &str, id_or_prefix: &str) -> Result<AgentRequest> {
        let mut matches: Vec<AgentRequest> = self
            .list_agent_requests(network)?
            .into_iter()
            .filter(|request| request.id.starts_with(id_or_prefix))
            .collect();
        if let Some(exact) = matches.iter().position(|r| r.id == id_or_prefix) {
            return Ok(matches.remove(exact));
        }
        match matches.len() {
            0 => bail!("no agent request {id_or_prefix}"),
            1 => Ok(matches.remove(0)),
            _ => bail!("request id {id_or_prefix} is ambiguous"),
        }
    }

    /// Every line of the event log, in append order, read tolerantly: a newer sats may
    /// have appended kinds or versions this build does not know, and the
    /// audit view must show them raw rather than hide or abort on them.
    /// Only a line that is not JSON at all is skipped, with a warning.
    pub fn list_event_lines(&self, network: &str) -> Result<Vec<EventLine>> {
        let path = self.events_dir(network).join("log.jsonl");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let contents =
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        let mut lines = Vec::new();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                eprintln!("⚠ skipping an event log line that is not JSON");
                continue;
            };
            match serde_json::from_value::<AgentEvent>(value.clone()) {
                Ok(event) if event.version_supported() => lines.push(EventLine::Event(event)),
                _ => lines.push(EventLine::Unknown(value)),
            }
        }
        Ok(lines)
    }

    /// Append one event to the network's causal log: single line of JSON,
    /// fsynced, under the log's own lock. Log order is the audit order.
    pub fn append_event(&self, network: &str, event: &AgentEvent) -> Result<()> {
        let dir = self.events_dir(network);
        self.create_private_dirs(&dir)?;
        let lock = fs::File::create(dir.join(".lock"))
            .with_context(|| format!("cannot open event lock in {}", dir.display()))?;
        lock.lock()
            .with_context(|| format!("cannot lock event log in {}", dir.display()))?;
        let path = dir.join("log.jsonl");
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        // The log names recipients and amounts: owner-only, every open.
        set_secret_perms(&file, true)?;
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        let mut file = file;
        file.write_all(&line)?;
        file.sync_all()?;
        Ok(())
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

/// Gate an agent name before it becomes a path component. Every store
/// function that joins an agent name into a path must pass it through
/// here: the daemon receives the name raw off its socket, and `join`
/// with `..` or an absolute path escapes the data directory entirely.
fn agent_component(agent: &str) -> Result<&str> {
    if !valid_agent_name(agent) {
        bail!("invalid agent name {agent:?}: {AGENT_NAME_RULE}");
    }
    Ok(agent)
}

/// Gate a request id before it becomes a path component. Daemon-minted
/// ids (`k-<key>`, `r-<hex>`) always pass; this is the backstop for any
/// future caller handing an id straight from a wire.
fn request_id_component(id: &str) -> Result<&str> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        bail!("invalid request id {id:?}: must be 1-128 chars of A-Za-z0-9_-");
    }
    Ok(id)
}

/// A grant written before the signing daemon existed: it carries the
/// master seed re-sealed under a key stored in the same file.
fn is_legacy_grant(value: &serde_json::Value) -> bool {
    value.get("wrapped_seed").is_some() || value.get("grant_key").is_some()
}

/// What a human must do about a v1 grant. It names the commands rather
/// than describing them, because the fix is two lines of shell.
pub fn legacy_grant_message(agent: &str, path: &Path) -> String {
    format!(
        "grant for {agent:?} uses the v1 format, which stored recoverable signing \
         material in the grant file itself. It cannot be used for signing.\n\n\
         Re-issue it:\n\n    \
         sats agent revoke {agent}\n    \
         sats agent grant {agent} --budget <sats> --for 24h\n\n\
         Then delete the old file: {}\n\n\
         If an agent with shell access ever ran on this machine while that grant \
         existed, treat the seed as disclosed and move the funds to a fresh wallet.",
        path.display()
    )
}

/// Restrict a file to its owner. Used for the daemon socket, where the
/// permission bits are the whole access-control story.
pub fn harden_file(path: &Path) -> Result<()> {
    harden_path(path).with_context(|| format!("cannot restrict permissions on {}", path.display()))
}

/// Restrict a directory to its owner.
#[cfg(unix)]
pub fn harden_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot restrict permissions on {}", path.display()))
}

#[cfg(not(unix))]
pub fn harden_dir(_path: &Path) -> Result<()> {
    Ok(())
}

/// Write via tmp file + fsync + rename so a crash never leaves a torn file.
/// `secret` restricts the file to owner read/write before any bytes land.
pub fn write_atomic(path: &Path, bytes: &[u8], secret: bool) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    // A secret file's directory listing is metadata about the secret:
    // keep the containing directory owner-only too.
    if secret {
        harden_dir(dir)?;
    }
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

/// Exclusive daemon ownership, including the stale-socket recovery window.
pub fn lock_daemon(socket: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let parent = socket.parent().context("socket has no parent")?;
    fs::create_dir_all(parent)?;
    harden_dir(parent)?;
    let path = socket.with_extension("lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    set_secret_perms(&file, true)?;
    file.try_lock().map_err(|e| {
        anyhow::anyhow!(
            "satsd is already running or starting for {}: {e}",
            socket.display()
        )
    })?;
    Ok(file)
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
    use sats_core::plan::{PreparedSpend, TransactionRecord};
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

    fn record_with(value_sat: u64, created_at: u64) -> TransactionRecord {
        let mut tx = test_transaction();
        tx.output[0].value = Amount::from_sat(value_sat);
        TransactionRecord::from_transaction(
            "signet".into(),
            "tb1ptest".into(),
            value_sat,
            100,
            created_at,
            0,
            None,
            &tx,
        )
    }

    fn grant(network: &str) -> Grant {
        let token = sats_core::token::generate().unwrap();
        Grant {
            format_version: sats_core::authz::GRANT_FORMAT_VERSION,
            agent: "claude".into(),
            network: network.into(),
            budget_sat: 50_000,
            spent_sat: 0,
            max_tx_sat: None,
            max_fee_sat: None,
            created_at: 0,
            expires_at: u64::MAX,
            tx_count: 0,
            token_id: token.token_id,
            token_hash: token.token_hash,
            mode: Default::default(),
            ask_max_tx_sat: None,
            allowed_recipients: None,
            suspended: None,
            strikes: Vec::new(),
        }
    }

    /// A grant authorizes only the network it names. The v1 seal bound the
    /// seed to the network by AAD, so a copied grant was inert; a v2 grant
    /// carries no seal, so `load_grant` is what keeps a signet grant
    /// dropped into the mainnet directory from authorizing mainnet signing.
    #[test]
    fn load_grant_refuses_a_grant_from_another_network() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();

        // The exploit: a real signet grant's file placed under mainnet.
        let signet_grant = grant("signet");
        store.save_grant("mainnet", &signet_grant).unwrap();

        let err = store.load_grant("mainnet", "claude").unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("does not match mainnet"),
            "cross-network grant must be refused, got: {message}"
        );

        // A grant filed under the network it names loads normally, and the
        // wrong directory simply has no grant rather than a stolen one.
        store.save_grant("signet", &grant("signet")).unwrap();
        assert!(store.load_grant("signet", "claude").unwrap().is_some());
        // (only the mainnet copy remains under mainnet, still refused)
        assert!(store.load_grant("mainnet", "claude").is_err());
    }

    /// Version discipline both ways: a v2 record (no v3 fields) loads
    /// with default semantics, and a record from the future is refused —
    /// it may carry restrictions this build cannot see, and ignoring
    /// them would widen authority.
    #[test]
    fn load_grant_reads_v2_and_refuses_newer_versions() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        store.save_grant("signet", &grant("signet")).unwrap();
        let path = store.grants_dir("signet").join("claude.json");

        // Strip the file down to its v2 shape.
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.insert("format_version".into(), serde_json::json!(2));
        object.remove("mode");
        fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        let loaded = store.load_grant("signet", "claude").unwrap().unwrap();
        assert_eq!(loaded.format_version, 2);
        assert_eq!(loaded.mode, sats_core::authz::GrantMode::Auto);
        assert_eq!(loaded.ask_max_tx_sat, None);
        assert!(loaded.suspended.is_none());

        // The future is refused, not partially honored.
        value.as_object_mut().unwrap().insert(
            "format_version".into(),
            serde_json::json!(sats_core::authz::GRANT_FORMAT_VERSION + 1),
        );
        fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        let err = store.load_grant("signet", "claude").unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("newer sats"),
            "future grant must be refused, got: {message}"
        );
    }

    /// The audit view survives the future: a line whose kind this build
    /// does not know is returned raw, never hidden and never fatal.
    #[test]
    fn event_lines_tolerate_unknown_kinds() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        let event = sats_core::event::AgentEvent {
            format_version: sats_core::event::EVENT_FORMAT_VERSION,
            at: 1,
            network: "signet".into(),
            agent: "claude".into(),
            request_id: "k-job-1".into(),
            intent_digest: "d".repeat(64),
            kind: sats_core::event::EventKind::Replayed,
        };
        store.append_event("signet", &event).unwrap();
        let log = store.events_dir("signet").join("log.jsonl");
        let mut contents = fs::read_to_string(&log).unwrap();
        contents.push_str(
            "{\"at\":2,\"network\":\"signet\",\"agent\":\"claude\",\
             \"request_id\":\"k-job-2\",\"intent_digest\":\"d\",\
             \"event\":\"quantum_settled\",\"detail\":42}\n",
        );
        contents.push_str("not json at all\n");
        fs::write(&log, contents).unwrap();

        let lines = store.list_event_lines("signet").unwrap();
        assert_eq!(lines.len(), 2, "the non-JSON line is skipped");
        assert!(matches!(&lines[0], EventLine::Event(e) if e.request_id == "k-job-1"));
        match &lines[1] {
            EventLine::Unknown(value) => {
                assert_eq!(value["event"], "quantum_settled");
                assert_eq!(value["request_id"], "k-job-2");
            }
            other => panic!("expected the unknown kind raw, got {other:?}"),
        }
    }

    #[test]
    fn grant_lock_excludes_a_second_holder() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        let held = store.lock_grants("signet").unwrap();
        // A separate open of the lock file must contend, even in-process.
        let second = fs::File::create(store.grants_dir("signet").join(".lock")).unwrap();
        assert!(
            second.try_lock().is_err(),
            "lock must exclude concurrent holders"
        );
        drop(held);
        second.try_lock().expect("lock must be free after drop");
    }

    fn agent_request(id: &str) -> AgentRequest {
        AgentRequest {
            format_version: sats_core::request::REQUEST_FORMAT_VERSION,
            id: id.into(),
            network: "signet".into(),
            agent: "claude".into(),
            client_request_id: None,
            recipient: "tb1ptest".into(),
            amount_sat: 1_000,
            intent_digest: "d".repeat(64),
            created_at: 42,
            updated_at: 42,
            outcome: None,
            approval: None,
            dismissed_at: None,
        }
    }

    #[test]
    fn agent_request_claim_is_exclusive_and_private() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        let request = agent_request("k-job-1");

        let claim = store
            .create_agent_request("signet", &request)
            .unwrap()
            .expect("first create claims");
        // A second create of the same id signals the idempotency hit.
        assert!(
            store
                .create_agent_request("signet", &request)
                .unwrap()
                .is_none()
        );
        // The execution lock is held: a concurrent claim must not succeed.
        assert!(
            store
                .claim_agent_request("signet", "claude", "k-job-1")
                .unwrap()
                .is_none()
        );
        drop(claim);
        let reclaim = store
            .claim_agent_request("signet", "claude", "k-job-1")
            .unwrap();
        assert!(reclaim.is_some(), "released lock must be claimable");

        let loaded = store
            .load_agent_request("signet", "claude", "k-job-1")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.id, "k-job-1");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = store
                .agent_requests_dir("signet")
                .join("claude")
                .join("k-job-1.json");
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn event_log_appends_lines_privately() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        let event = sats_core::event::AgentEvent {
            format_version: sats_core::event::EVENT_FORMAT_VERSION,
            at: 1,
            network: "signet".into(),
            agent: "claude".into(),
            request_id: "k-job-1".into(),
            intent_digest: "d".repeat(64),
            kind: sats_core::event::EventKind::Replayed,
        };
        store.append_event("signet", &event).unwrap();
        store.append_event("signet", &event).unwrap();
        let path = store.events_dir("signet").join("log.jsonl");
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 2);
        for line in contents.lines() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["event"], "replayed");
        }
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
    fn listing_transactions_skips_unreadable_files() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        store
            .save_transaction("signet", &record_with(1_000, 10))
            .unwrap();
        store
            .save_transaction("signet", &record_with(2_000, 20))
            .unwrap();
        fs::write(
            store.transactions_dir("signet").join("garbage.json"),
            b"not json",
        )
        .unwrap();

        let records = store.list_transactions("signet").unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].created_at, 20);
        assert_eq!(records[1].created_at, 10);
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

    /// Agent names reach the store raw off the daemon socket, so every
    /// path join gates them: a traversal name must fail and leave no
    /// artifact anywhere.
    #[test]
    fn traversal_agent_names_are_rejected_at_the_store() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();

        for evil in ["../evil", "a/b", "", "/etc", "A", "a\\b"] {
            assert!(store.load_grant("signet", evil).is_err(), "load {evil:?}");
            assert!(
                store.delete_grant("signet", evil).is_err(),
                "delete {evil:?}"
            );
            let mut g = grant("signet");
            g.agent = evil.into();
            assert!(store.save_grant("signet", &g).is_err(), "save {evil:?}");

            let mut request = agent_request("k-job-1");
            request.agent = evil.into();
            assert!(
                store.create_agent_request("signet", &request).is_err(),
                "create request {evil:?}"
            );
            assert!(
                store
                    .claim_agent_request("signet", evil, "k-job-1")
                    .is_err(),
                "claim {evil:?}"
            );
            assert!(
                store.load_agent_request("signet", evil, "k-job-1").is_err(),
                "load request {evil:?}"
            );
        }
        // A hostile request id is refused the same way.
        assert!(
            store
                .create_agent_request("signet", &agent_request("../../evil"))
                .is_err()
        );

        // Nothing escaped the data directory, and nothing was created for
        // any of the refused names.
        assert!(!dir.path().parent().unwrap().join("evil").exists());
        assert!(!dir.path().join("evil").exists());
        assert!(!store.agent_requests_dir("signet").exists());
    }

    /// Reading grant state must not mutate it: the daemon's status op is
    /// unauthenticated, so listing leaves expired files alone and only the
    /// explicit prune removes them.
    #[test]
    fn listing_does_not_delete_expired_grants_but_prune_does() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        let mut expired = grant("signet");
        expired.expires_at = 100;
        store.save_grant("signet", &expired).unwrap();
        let path = store.grants_dir("signet").join("claude.json");

        let listed = store.active_grants("signet", 1_000).unwrap();
        assert!(listed.is_empty(), "expired grant must not list as active");
        assert!(path.exists(), "listing must not delete the file");

        let pruned = store.prune_expired_grants("signet", 1_000).unwrap();
        assert_eq!(pruned, vec!["claude".to_string()]);
        assert!(!path.exists(), "prune removes the expired file");

        // An unexpired grant survives both.
        store.save_grant("signet", &grant("signet")).unwrap();
        assert!(
            store
                .prune_expired_grants("signet", 1_000)
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.active_grants("signet", 1_000).unwrap().len(), 1);
    }

    /// Directory listings under the data dir name agents, txids, and
    /// request ids: every component down from the data dir is owner-only.
    #[cfg(unix)]
    #[test]
    fn data_directories_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let store = Store::open(Some(dir.path())).unwrap();
        store.save_grant("signet", &grant("signet")).unwrap();
        store
            .create_agent_request("signet", &agent_request("k-job-1"))
            .unwrap()
            .unwrap();

        for private in [
            store.grants_dir("signet"),
            store.agent_requests_dir("signet"),
            store.agent_requests_dir("signet").join("claude"),
            dir.path().join("signet"),
        ] {
            assert_eq!(
                fs::metadata(&private).unwrap().permissions().mode() & 0o777,
                0o700,
                "{} must be owner-only",
                private.display()
            );
        }
    }
}
