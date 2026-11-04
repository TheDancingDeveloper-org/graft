//! The `jti` denylist (`docs/design.md` §12.1): revoked token ids, checked
//! on writes, and the record of which service minted a token over the API,
//! which decides who may revoke it.
//!
//! The table lives in the metadata database, beside the MetaStore's own
//! tables, so `admin backup` snapshots it with everything else. The server
//! owns it: it is created here, not by the store's migrations. A row can be
//! dropped once its token has expired, because an expired token is refused
//! anyway, so the table stays small.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension as _, params};

/// How long a write waits for the metadata database's lock.
const BUSY: Duration = Duration::from_secs(5);

/// The revoked token ids, in the metadata database.
pub(crate) struct Denylist {
    conn: Mutex<Connection>,
}

impl Denylist {
    /// Opens (and creates, if needed) the denylist in `meta_db`. The
    /// MetaStore must have opened the file first, so its schema exists.
    pub(crate) fn open(meta_db: &Path) -> Result<Self, String> {
        let conn = Connection::open(meta_db).map_err(|e| format!("denylist: {e}"))?;
        conn.busy_timeout(BUSY)
            .map_err(|e| format!("denylist: {e}"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS token_denylist (
                 jti TEXT PRIMARY KEY NOT NULL,
                 exp INTEGER NOT NULL,
                 revoked_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS token_mints (
                 jti TEXT PRIMARY KEY NOT NULL,
                 minter TEXT NOT NULL,
                 exp INTEGER NOT NULL
             );",
        )
        .map_err(|e| format!("denylist: {e}"))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Revokes `jti`, which expires at `exp` (seconds since the epoch), and
    /// drops rows whose tokens have already expired. Revoking twice is fine.
    pub(crate) fn revoke(&self, jti: &str, exp: i64, now: i64) -> Result<(), String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT OR IGNORE INTO token_denylist (jti, exp, revoked_at) VALUES (?1, ?2, ?3)",
            params![jti, exp, now],
        )
        .map_err(|e| format!("denylist: {e}"))?;
        conn.execute("DELETE FROM token_denylist WHERE exp < ?1", params![now])
            .map_err(|e| format!("denylist: {e}"))?;
        Ok(())
    }

    /// Records that the service principal `minter` minted token `jti`, which
    /// expires at `exp`, and drops records of tokens that have expired. Only
    /// tokens minted through the API are recorded; `admin token mint` tokens
    /// have no minter and only an admin revokes them.
    pub(crate) fn record_mint(
        &self,
        jti: &str,
        minter: &str,
        exp: i64,
        now: i64,
    ) -> Result<(), String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT OR IGNORE INTO token_mints (jti, minter, exp) VALUES (?1, ?2, ?3)",
            params![jti, minter, exp],
        )
        .map_err(|e| format!("denylist: {e}"))?;
        conn.execute("DELETE FROM token_mints WHERE exp < ?1", params![now])
            .map_err(|e| format!("denylist: {e}"))?;
        Ok(())
    }

    /// Whether the service principal `minter` minted token `jti` through the
    /// API. A database error is an error, not "no": callers refuse.
    pub(crate) fn minted_by(&self, jti: &str, minter: &str) -> Result<bool, String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM token_mints WHERE jti = ?1 AND minter = ?2",
                params![jti, minter],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("denylist: {e}"))?;
        Ok(found.is_some())
    }

    /// Whether `jti` is revoked. A database error is an error, not "not
    /// revoked": callers refuse the write when they cannot tell.
    pub(crate) fn is_revoked(&self, jti: &str) -> Result<bool, String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM token_denylist WHERE jti = ?1",
                params![jti],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("denylist: {e}"))?;
        Ok(found.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revoked_ids_are_found_and_expired_rows_are_dropped() {
        let dir = graft_testkit::crash::TempDir::new("denylist");
        let path = dir.path().join("meta.db");
        let deny = Denylist::open(&path).expect("open");
        assert!(!deny.is_revoked("a").expect("read"));
        deny.revoke("a", 100, 10).expect("revoke");
        deny.revoke("a", 100, 11).expect("revoking twice is fine");
        assert!(deny.is_revoked("a").expect("read"));
        assert!(!deny.is_revoked("b").expect("read"));
        // A later revoke, after `a` expired, drops it.
        deny.revoke("c", 500, 200).expect("revoke");
        assert!(!deny.is_revoked("a").expect("read"));
        assert!(deny.is_revoked("c").expect("read"));
        // Reopening sees the same rows.
        let again = Denylist::open(&path).expect("reopen");
        assert!(again.is_revoked("c").expect("read"));
    }

    #[test]
    fn mint_records_name_the_minter_and_are_dropped_on_expiry() {
        let dir = graft_testkit::crash::TempDir::new("denylist-mints");
        let path = dir.path().join("meta.db");
        let deny = Denylist::open(&path).expect("open");
        deny.record_mint("a", "vogt", 100, 10).expect("record");
        assert!(deny.minted_by("a", "vogt").expect("read"));
        assert!(!deny.minted_by("a", "other").expect("read"));
        assert!(!deny.minted_by("b", "vogt").expect("read"));
        // The first record stands; a second claim on the same jti is ignored.
        deny.record_mint("a", "other", 100, 11).expect("record");
        assert!(!deny.minted_by("a", "other").expect("read"));
        // A later record, after `a` expired, drops it.
        deny.record_mint("c", "vogt", 500, 200).expect("record");
        assert!(!deny.minted_by("a", "vogt").expect("read"));
        assert!(
            Denylist::open(&path)
                .expect("reopen")
                .minted_by("c", "vogt")
                .expect("read")
        );
    }
}
