//! Exported commits (`docs/design.md` §11.2, §12.1): which commit
//! graft-mirror exported for which changeset, recorded by the mirror through
//! `MirrorService.RecordExports`. A pinned CI exchange reads it to check that
//! the job ran on the commit exported for the changeset it attests.
//!
//! The server cannot work this out for itself. A native changeset's commit
//! carries a committer date the mirror chooses (the land request's planning
//! time, or when the export first wrote it), and its parents' git trees and
//! imported commit headers live only in the mirror's database and bare
//! repository. So the mirror, which holds the admin token for the
//! repository, tells the server.
//!
//! The table lives in the metadata database beside the denylist, so
//! `admin backup` snapshots it. Commits are opaque text here: whatever the CI
//! reports as the job's commit (lowercase hex). A changeset may have more than
//! one recorded commit (exported again by another mirror); any of them
//! matches. Rows are keyed by repository, so an admin of one repository
//! cannot vouch for a commit in another.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use graft_model::ChangesetId;
use rusqlite::{Connection, OptionalExtension as _, params};

/// How long a write waits for the metadata database's lock.
const BUSY: Duration = Duration::from_secs(5);

/// The most exports one `RecordExports` call may carry.
pub(crate) const MAX_RECORDS: usize = 256;

/// The longest commit id accepted: a SHA-256 in hex.
const MAX_COMMIT_LEN: usize = 64;

/// A commit id as stored and compared: hex digits, lowercased. `None` when
/// `text` is empty, too long, or not hex.
pub(crate) fn commit_id(text: &str) -> Option<String> {
    (!text.is_empty()
        && text.len() <= MAX_COMMIT_LEN
        && text.bytes().all(|b| b.is_ascii_hexdigit()))
    .then(|| text.to_ascii_lowercase())
}

/// The recorded exports, in the metadata database.
pub(crate) struct Exports {
    conn: Mutex<Connection>,
}

impl Exports {
    /// Opens (and creates, if needed) the table in `meta_db`. The MetaStore
    /// must have opened the file first, so its schema exists.
    pub(crate) fn open(meta_db: &Path) -> Result<Self, String> {
        let conn = Connection::open(meta_db).map_err(|e| format!("exports: {e}"))?;
        conn.busy_timeout(BUSY)
            .map_err(|e| format!("exports: {e}"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS exported_commits (
                 repo TEXT NOT NULL,
                 changeset TEXT NOT NULL,
                 commit_id TEXT NOT NULL,
                 recorded_at INTEGER NOT NULL,
                 PRIMARY KEY (repo, changeset, commit_id)
             ) WITHOUT ROWID;",
        )
        .map_err(|e| format!("exports: {e}"))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Records each `(changeset, commit)` of `repo`, in one transaction.
    /// Commits must already be [`commit_id`]s. Recording a pair twice keeps
    /// the first.
    pub(crate) fn record(
        &self,
        repo: &str,
        exports: &[(ChangesetId, String)],
        now_s: i64,
    ) -> Result<(), String> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction().map_err(|e| format!("exports: {e}"))?;
        {
            let mut insert = tx
                .prepare_cached(
                    "INSERT OR IGNORE INTO exported_commits
                         (repo, changeset, commit_id, recorded_at) VALUES (?1, ?2, ?3, ?4)",
                )
                .map_err(|e| format!("exports: {e}"))?;
            for (changeset, commit) in exports {
                insert
                    .execute(params![repo, changeset.to_string(), commit, now_s])
                    .map_err(|e| format!("exports: {e}"))?;
            }
        }
        tx.commit().map_err(|e| format!("exports: {e}"))
    }

    /// Whether `commit` was recorded as an export of `changeset` in `repo`.
    /// A database error is an error, not "no": callers refuse when they
    /// cannot tell.
    pub(crate) fn is_export_of(
        &self,
        repo: &str,
        changeset: ChangesetId,
        commit: &str,
    ) -> Result<bool, String> {
        let Some(commit) = commit_id(commit) else {
            return Ok(false);
        };
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM exported_commits
                     WHERE repo = ?1 AND changeset = ?2 AND commit_id = ?3",
                params![repo, changeset.to_string(), commit],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("exports: {e}"))?;
        Ok(found.is_some())
    }
}

#[cfg(test)]
mod tests {
    use graft_model::ObjectId;

    use super::*;

    const SHA: &str = "1f2e3d4c5b6a79881f2e3d4c5b6a79881f2e3d4c";

    #[test]
    fn commit_ids_are_hex_and_lowercased() {
        assert_eq!(commit_id(&SHA.to_uppercase()).as_deref(), Some(SHA));
        assert_eq!(
            commit_id(&"a".repeat(64)).as_deref(),
            Some(&*"a".repeat(64))
        );
        for bad in ["", "xyz", "12 34", &"a".repeat(65), "1f2e\n"] {
            assert_eq!(commit_id(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn exports_are_found_per_repository_and_survive_a_reopen() {
        let dir = graft_testkit::crash::TempDir::new("exports");
        let path = dir.path().join("meta.db");
        let exports = Exports::open(&path).expect("open");
        let a = ChangesetId(ObjectId([1; 32]));
        let b = ChangesetId(ObjectId([2; 32]));
        assert!(!exports.is_export_of("r", a, SHA).expect("read"));
        exports
            .record("r", &[(a, SHA.to_owned())], 10)
            .expect("record");
        exports
            .record("r", &[(a, SHA.to_owned())], 11)
            .expect("recording twice is fine");
        assert!(exports.is_export_of("r", a, SHA).expect("read"));
        assert!(
            exports
                .is_export_of("r", a, &SHA.to_uppercase())
                .expect("read")
        );
        // Another changeset, another commit, another repository: no.
        assert!(!exports.is_export_of("r", b, SHA).expect("read"));
        let other = SHA.replace('1', "0");
        assert!(!exports.is_export_of("r", a, &other).expect("read"));
        assert!(!exports.is_export_of("other", a, SHA).expect("read"));
        // A second commit for the same changeset is kept beside the first.
        exports
            .record("r", &[(a, other.clone())], 12)
            .expect("record");
        assert!(exports.is_export_of("r", a, SHA).expect("read"));
        assert!(exports.is_export_of("r", a, &other).expect("read"));
        // Reopening sees the same rows.
        let again = Exports::open(&path).expect("reopen");
        assert!(again.is_export_of("r", a, SHA).expect("read"));
        assert!(!again.is_export_of("r", a, "not hex").expect("read"));
    }
}
