//! The relay's only state: sealed filings waiting for a device, and the
//! results devices posted back, sealed for the agent's connection. Losing this file loses pending requests
//! and observations, never money or authority: no key, wallet, or grant
//! policy lives here.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// Filings one mailbox may hold at once, bounding a flood of requests.
pub const MAX_PENDING_PER_MAILBOX: i64 = 64;

/// Statuses after which a device has nothing left to report.
pub const TERMINAL: [&str; 4] = ["denied", "dismissed", "sent", "error"];

pub struct Mailbox {
    conn: Mutex<Connection>,
    /// Seconds a filing waits for a device before it is dropped.
    pub filing_ttl: u64,
    /// Seconds a posted result stays observable.
    pub result_ttl: u64,
}

/// What the relay knows about one request id.
pub enum Known {
    /// A device posted a result.
    Result {
        human: String,
        agent: String,
        digest: String,
        /// Readable, so finished filings can be cleared.
        status: String,
        /// The view, sealed under the grant's result key; `None` when the
        /// device refused a filing it couldn't attribute to a grant.
        sealed: Option<Vec<u8>>,
    },
    /// Filed and waiting for the device.
    Pending {
        human: String,
        agent: String,
        digest: String,
    },
}

pub struct NewFiling<'a> {
    pub request_id: &'a str,
    pub human: &'a str,
    pub agent: &'a str,
    pub digest: &'a str,
    pub sealed: &'a [u8],
    pub now: u64,
}

pub enum Filed {
    Stored,
    MailboxFull,
}

/// Why a posted result was not stored.
#[derive(Debug, PartialEq, Eq)]
pub enum Rejected {
    /// No filing or result for this id in the caller's mailbox.
    Unknown,
    /// Not newer than what the relay already holds.
    Stale,
}

impl Mailbox {
    pub fn open(path: &Path, filing_ttl: u64, result_ttl: u64) -> Result<Mailbox> {
        let conn =
            Connection::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        // The OAuth store shares this file.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS filings (
                 request_id TEXT PRIMARY KEY,
                 human TEXT NOT NULL,
                 agent TEXT NOT NULL,
                 digest TEXT NOT NULL,
                 sealed BLOB NOT NULL,
                 filed_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS filings_by_human ON filings (human, filed_at);
             -- Results in readable form, from before sealing: dropped. They
             -- are observations only, and devices re-post on the next sync.
             DROP TABLE IF EXISTS results;
             CREATE TABLE IF NOT EXISTS sealed_results (
                 request_id TEXT PRIMARY KEY,
                 human TEXT NOT NULL,
                 agent TEXT NOT NULL,
                 digest TEXT NOT NULL,
                 revision INTEGER NOT NULL,
                 status TEXT NOT NULL,
                 sealed BLOB,
                 updated_at INTEGER NOT NULL
             );",
        )
        .context("cannot prepare the mailbox database")?;
        Ok(Mailbox {
            conn: Mutex::new(conn),
            filing_ttl,
            result_ttl,
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves SQLite consistent: every
        // write is a single statement or transaction.
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn sweep(&self, conn: &Connection, now: u64) -> Result<()> {
        conn.execute(
            "DELETE FROM filings WHERE filed_at < ?1",
            params![now.saturating_sub(self.filing_ttl) as i64],
        )?;
        conn.execute(
            "DELETE FROM sealed_results WHERE updated_at < ?1",
            params![now.saturating_sub(self.result_ttl) as i64],
        )?;
        Ok(())
    }

    pub fn lookup(&self, request_id: &str) -> Result<Option<Known>> {
        let conn = self.conn();
        let result = conn
            .query_row(
                "SELECT human, agent, digest, status, sealed FROM sealed_results WHERE request_id = ?1",
                params![request_id],
                |row| {
                    Ok(Known::Result {
                        human: row.get(0)?,
                        agent: row.get(1)?,
                        digest: row.get(2)?,
                        status: row.get(3)?,
                        sealed: row.get(4)?,
                    })
                },
            )
            .optional()?;
        if result.is_some() {
            return Ok(result);
        }
        Ok(conn
            .query_row(
                "SELECT human, agent, digest FROM filings WHERE request_id = ?1",
                params![request_id],
                |row| {
                    Ok(Known::Pending {
                        human: row.get(0)?,
                        agent: row.get(1)?,
                        digest: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn file(&self, filing: &NewFiling) -> Result<Filed> {
        let mut conn = self.conn();
        self.sweep(&conn, filing.now)?;
        let tx = conn.transaction()?;
        let pending: i64 = tx.query_row(
            "SELECT COUNT(*) FROM filings WHERE human = ?1",
            params![filing.human],
            |row| row.get(0),
        )?;
        if pending >= MAX_PENDING_PER_MAILBOX {
            return Ok(Filed::MailboxFull);
        }
        tx.execute(
            "INSERT OR IGNORE INTO filings (request_id, human, agent, digest, sealed, filed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                filing.request_id,
                filing.human,
                filing.agent,
                filing.digest,
                filing.sealed,
                filing.now as i64
            ],
        )?;
        tx.commit()?;
        Ok(Filed::Stored)
    }

    /// Sealed filings waiting for `human`, oldest first.
    pub fn inbox(&self, human: &str, now: u64) -> Result<Vec<(String, Vec<u8>)>> {
        let conn = self.conn();
        self.sweep(&conn, now)?;
        let mut statement = conn.prepare(
            "SELECT request_id, sealed FROM filings WHERE human = ?1 ORDER BY filed_at, request_id",
        )?;
        let rows = statement
            .query_map(params![human], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Store a device's result for one of its own filings. A terminal
    /// result removes the sealed filing; the result keeps the digest so a
    /// reused idempotency key still conflicts.
    pub fn post_result(
        &self,
        human: &str,
        request_id: &str,
        revision: u64,
        status: &str,
        sealed: Option<&[u8]>,
        now: u64,
    ) -> Result<Result<(), Rejected>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let owner: Option<(String, String, Option<i64>)> = tx
            .query_row(
                "SELECT agent, digest, NULL FROM filings WHERE request_id = ?1 AND human = ?2
                 UNION ALL
                 SELECT agent, digest, revision FROM sealed_results WHERE request_id = ?1 AND human = ?2
                 ORDER BY 3 DESC",
                params![request_id, human],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((agent, digest, stored_revision)) = owner else {
            return Ok(Err(Rejected::Unknown));
        };
        if stored_revision.is_some_and(|stored| stored >= revision as i64) {
            return Ok(Err(Rejected::Stale));
        }
        tx.execute(
            "INSERT INTO sealed_results
                 (request_id, human, agent, digest, revision, status, sealed, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (request_id) DO UPDATE SET
                 revision = excluded.revision, status = excluded.status,
                 sealed = excluded.sealed, updated_at = excluded.updated_at",
            params![
                request_id,
                human,
                agent,
                digest,
                revision as i64,
                status,
                sealed,
                now as i64
            ],
        )?;
        if TERMINAL.contains(&status) {
            tx.execute(
                "DELETE FROM filings WHERE request_id = ?1",
                params![request_id],
            )?;
        }
        tx.commit()?;
        Ok(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mailbox() -> (tempfile::TempDir, Mailbox) {
        let dir = tempfile::TempDir::new().unwrap();
        let mailbox = Mailbox::open(&dir.path().join("relay.db"), 100, 1000).unwrap();
        (dir, mailbox)
    }

    fn file(mailbox: &Mailbox, id: &str, human: &str, now: u64) -> Filed {
        mailbox
            .file(&NewFiling {
                request_id: id,
                human,
                agent: "chatgpt",
                digest: "d",
                sealed: b"sealed",
                now,
            })
            .unwrap()
    }

    #[test]
    fn results_are_owned_monotonic_and_terminal_results_clear_the_filing() {
        let (_dir, mailbox) = mailbox();
        assert!(matches!(file(&mailbox, "r-1", "h1", 10), Filed::Stored));
        let pending = ("pending_approval", Some(b"sealed-pending".as_slice()));
        // Another device can't answer for this filing.
        assert_eq!(
            mailbox
                .post_result("h2", "r-1", 1, pending.0, pending.1, 10)
                .unwrap(),
            Err(Rejected::Unknown)
        );
        assert_eq!(
            mailbox
                .post_result("h1", "r-1", 5, pending.0, pending.1, 10)
                .unwrap(),
            Ok(())
        );
        assert_eq!(mailbox.inbox("h1", 10).unwrap().len(), 1);
        // A replayed or older result can't roll the state back.
        assert_eq!(
            mailbox
                .post_result("h1", "r-1", 5, pending.0, pending.1, 11)
                .unwrap(),
            Err(Rejected::Stale)
        );
        let sent = ("sent", Some(b"sealed-sent".as_slice()));
        assert_eq!(
            mailbox
                .post_result("h1", "r-1", 6, sent.0, sent.1, 11)
                .unwrap(),
            Ok(())
        );
        assert!(mailbox.inbox("h1", 11).unwrap().is_empty());
        assert_eq!(
            mailbox
                .post_result("h1", "r-1", 4, pending.0, pending.1, 12)
                .unwrap(),
            Err(Rejected::Stale)
        );
        match mailbox.lookup("r-1").unwrap() {
            Some(Known::Result {
                status,
                sealed,
                digest,
                ..
            }) => {
                assert_eq!(status, "sent");
                assert_eq!(sealed.as_deref(), Some(b"sealed-sent".as_slice()));
                assert_eq!(digest, "d");
            }
            _ => panic!("expected a result"),
        }
    }

    #[test]
    fn mailboxes_are_bounded_and_expire() {
        let (_dir, mailbox) = mailbox();
        for i in 0..MAX_PENDING_PER_MAILBOX {
            assert!(matches!(
                file(&mailbox, &format!("r-{i}"), "h1", 10),
                Filed::Stored
            ));
        }
        assert!(matches!(
            file(&mailbox, "r-x", "h1", 10),
            Filed::MailboxFull
        ));
        // Other mailboxes are unaffected.
        assert!(matches!(file(&mailbox, "r-y", "h2", 10), Filed::Stored));
        // After the filing TTL the queue drains on its own.
        assert!(mailbox.inbox("h1", 111).unwrap().is_empty());
        assert!(matches!(file(&mailbox, "r-x", "h1", 111), Filed::Stored));
    }
}
