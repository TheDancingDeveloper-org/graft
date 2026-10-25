//! The mirror DB (`docs/design.md` §11.1 step 5, §11.3): a SQLite file the
//! mirror owns, outside the graft server. It holds the mirror map (graft ids
//! ↔ git shas, raw commit headers, raw trees, raw tag objects, blob shas)
//! and, per followed line, how far follow has got.
//!
//! Every write is one statement or one transaction, so a crash leaves either
//! the old row or the new one. The import records a commit only after the
//! server has accepted its changeset, so a restart re-walks at most the batch
//! that was in flight, and the server answers those retries idempotently.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use graft_git::map::{ImportedCommit, ImportedTag, MirrorMap, RawTree};
use graft_git::{GitError, GitSha};
use graft_model::{BlobId, ChangesetId, ObjectId, RepoPath};

use crate::batch::{BatchCommit, BatchLine, BatchRecord, BatchState};
use crate::pulls::{ImportState, PrImport};
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS commits (
    changeset BLOB PRIMARY KEY,
    sha BLOB NOT NULL UNIQUE,
    headers BLOB NOT NULL,
    message BLOB,
    raw_trees BLOB NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS tags (
    name TEXT PRIMARY KEY,
    sha BLOB NOT NULL,
    objects BLOB NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS blobs (
    blob BLOB PRIMARY KEY,
    sha BLOB NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS lines (
    line TEXT PRIMARY KEY,
    upstream BLOB,
    behind_since_ms INTEGER,
    diverged BLOB
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS authority (
    line TEXT PRIMARY KEY,
    claim TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS switches (
    line TEXT PRIMARY KEY,
    from_claim TEXT NOT NULL,
    to_claim TEXT NOT NULL,
    stage TEXT NOT NULL,
    changeset BLOB,
    sha BLOB
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS exports (
    line TEXT PRIMARY KEY,
    lsn INTEGER NOT NULL,
    changeset BLOB,
    sha BLOB,
    version INTEGER NOT NULL,
    diverged INTEGER NOT NULL,
    stray BLOB
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS batches (
    number INTEGER PRIMARY KEY,
    line TEXT NOT NULL,
    base_branch TEXT NOT NULL,
    base BLOB NOT NULL,
    base_sha BLOB NOT NULL,
    start BLOB NOT NULL,
    tip BLOB NOT NULL,
    lands INTEGER NOT NULL,
    initial_lands INTEGER NOT NULL,
    created_ms INTEGER NOT NULL,
    branch TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    squash_sha BLOB NOT NULL,
    pr_number INTEGER,
    pr_url TEXT,
    state TEXT NOT NULL,
    detail TEXT NOT NULL,
    merge_sha BLOB,
    merged BLOB
);
CREATE INDEX IF NOT EXISTS batches_by_line ON batches (line, number);
CREATE TABLE IF NOT EXISTS batch_lines (
    line TEXT PRIMARY KEY,
    synced BLOB NOT NULL,
    sync_from BLOB NOT NULL,
    start BLOB NOT NULL,
    start_base BLOB,
    sync_batch INTEGER
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS batch_drops (
    line TEXT NOT NULL,
    land BLOB NOT NULL,
    revert BLOB NOT NULL,
    reason TEXT NOT NULL,
    PRIMARY KEY (line, land)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS batch_commits (
    batch INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    land BLOB NOT NULL,
    sha BLOB NOT NULL,
    merged_sha BLOB,
    PRIMARY KEY (batch, seq)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS pr_imports (
    number INTEGER PRIMARY KEY,
    head_sha BLOB NOT NULL,
    workspace TEXT NOT NULL,
    head BLOB NOT NULL,
    state TEXT NOT NULL,
    detail TEXT NOT NULL,
    landed BLOB
);
";

/// Follow's record of one line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineState {
    /// The upstream commit the graft line was last moved to.
    pub upstream: Option<GitSha>,
    /// When follow first saw upstream ahead of graft, while it still is.
    pub behind_since_ms: Option<i64>,
    /// The upstream commit that is not a fast-forward of the graft line,
    /// while the line is stopped.
    pub diverged: Option<GitSha>,
}

/// Export's record of one graft-primary line (`docs/design.md` §11.3,
/// mode B): how far it has read the journal and what upstream holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportState {
    /// The next journal LSN to read.
    pub lsn: u64,
    /// The changeset upstream's branch was last pushed to.
    pub changeset: Option<ChangesetId>,
    /// Its git sha: the lease for the next push (`None`: no branch yet).
    pub sha: Option<GitSha>,
    /// The ref version of the last line move exported. Moves at or below it
    /// are already on upstream.
    pub version: u64,
    /// Whether the line is stopped on a refused lease.
    pub diverged: bool,
    /// What upstream held when the lease was refused (`None`: no branch).
    pub stray: Option<GitSha>,
}

/// An authority switch in progress on one line (`docs/design.md` §11.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchRecord {
    /// The claim that held the line when the switch began.
    pub from: String,
    /// The claim the switch hands the line to.
    pub to: String,
    /// `frozen`, `verified` or `flipped`.
    pub stage: String,
    /// The graft tip `verify` checked, once verified.
    pub changeset: Option<ChangesetId>,
    /// The upstream commit `verify` checked, once verified.
    pub sha: Option<GitSha>,
}

/// The mirror DB. One per mirrored graft repository.
pub struct MirrorDb {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl std::fmt::Debug for MirrorDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorDb").finish_non_exhaustive()
    }
}

fn db_err(e: rusqlite::Error) -> GitError {
    GitError::Map(format!("mirror db: {e}"))
}

fn sha_col(bytes: Option<Vec<u8>>) -> Result<Option<GitSha>, GitError> {
    bytes
        .map(|b| {
            <[u8; 20]>::try_from(b.as_slice())
                .map(GitSha)
                .map_err(|_| GitError::Map("mirror db: malformed sha".to_owned()))
        })
        .transpose()
}

fn id_col(bytes: Vec<u8>) -> Result<ObjectId, GitError> {
    <[u8; 32]>::try_from(bytes.as_slice())
        .map(ObjectId)
        .map_err(|_| GitError::Map("mirror db: malformed id".to_owned()))
}

impl MirrorDb {
    /// Opens (creating if absent) the DB at `path`.
    pub fn open(path: &Path) -> Result<MirrorDb, GitError> {
        let conn = Connection::open(path).map_err(db_err)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(db_err)?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(db_err)?;
        conn.execute_batch(SCHEMA).map_err(db_err)?;
        add_column(&conn, "batch_lines", "start_base", "BLOB")?;
        add_column(&conn, "batch_lines", "sync_batch", "INTEGER")?;
        add_column(
            &conn,
            "batches",
            "initial_lands",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        conn.execute(
            "UPDATE batches SET initial_lands = lands WHERE initial_lands = 0",
            [],
        )
        .map_err(db_err)?;
        Ok(MirrorDb {
            conn: Mutex::new(conn),
            path: path.to_owned(),
        })
    }

    /// The file this DB was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().expect("mirror db lock")
    }

    /// The number of imported commits recorded.
    pub fn commit_count(&self) -> Result<u64, GitError> {
        self.conn()
            .query_row("SELECT count(*) FROM commits", [], |r| r.get::<_, i64>(0))
            .map(|n| n as u64)
            .map_err(db_err)
    }

    /// Follow's record of `line` (default when it has none).
    pub fn line(&self, line: &str) -> Result<LineState, GitError> {
        let row = self
            .conn()
            .query_row(
                "SELECT upstream, behind_since_ms, diverged FROM lines WHERE line = ?1",
                params![line],
                |r| {
                    Ok((
                        r.get::<_, Option<Vec<u8>>>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<Vec<u8>>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(db_err)?;
        match row {
            None => Ok(LineState::default()),
            Some((upstream, behind, diverged)) => Ok(LineState {
                upstream: sha_col(upstream)?,
                behind_since_ms: behind,
                diverged: sha_col(diverged)?,
            }),
        }
    }

    /// Records `claim` as the authority over `line`, unless another claim
    /// is already there. Returns the claim that holds the line now. Two
    /// writers never both hold one line (`docs/design.md` §11.3).
    pub fn claim_authority(&self, line: &str, claim: &str) -> Result<String, GitError> {
        let conn = self.conn();
        conn.execute(
            "INSERT OR IGNORE INTO authority (line, claim) VALUES (?1, ?2)",
            params![line, claim],
        )
        .map_err(db_err)?;
        conn.query_row(
            "SELECT claim FROM authority WHERE line = ?1",
            params![line],
            |r| r.get::<_, String>(0),
        )
        .map_err(db_err)
    }

    /// The claim that holds `line`, if any. Reads only; never claims.
    pub fn authority(&self, line: &str) -> Result<Option<String>, GitError> {
        self.conn()
            .query_row(
                "SELECT claim FROM authority WHERE line = ?1",
                params![line],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(db_err)
    }

    /// The switch in progress on `line`, if any.
    pub fn switch(&self, line: &str) -> Result<Option<SwitchRecord>, GitError> {
        type Row = (String, String, String, Option<Vec<u8>>, Option<Vec<u8>>);
        let row: Option<Row> = self
            .conn()
            .query_row(
                "SELECT from_claim, to_claim, stage, changeset, sha FROM switches
                 WHERE line = ?1",
                params![line],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .map_err(db_err)?;
        let Some((from, to, stage, changeset, sha)) = row else {
            return Ok(None);
        };
        Ok(Some(SwitchRecord {
            from,
            to,
            stage,
            changeset: changeset.map(|b| id_col(b).map(ChangesetId)).transpose()?,
            sha: sha_col(sha)?,
        }))
    }

    /// Writes the switch record of `line`, replacing any earlier one.
    pub fn put_switch(&self, line: &str, rec: &SwitchRecord) -> Result<(), GitError> {
        self.conn()
            .execute(
                "INSERT INTO switches (line, from_claim, to_claim, stage, changeset, sha)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (line) DO UPDATE SET from_claim = ?2, to_claim = ?3, stage = ?4,
                     changeset = ?5, sha = ?6",
                params![
                    line,
                    rec.from,
                    rec.to,
                    rec.stage,
                    rec.changeset.map(|c| c.0.0.to_vec()),
                    rec.sha.map(|s| s.0.to_vec()),
                ],
            )
            .map(drop)
            .map_err(db_err)
    }

    /// Ends the switch on `line`: its record goes.
    pub fn end_switch(&self, line: &str) -> Result<(), GitError> {
        self.conn()
            .execute("DELETE FROM switches WHERE line = ?1", params![line])
            .map(drop)
            .map_err(db_err)
    }

    /// Hands `line` from claim `from` to claim `to`, in one transaction:
    /// the claim moves by compare-and-swap (`false`, and nothing changes,
    /// when `from` no longer holds it), export's record is dropped so the
    /// next export starts afresh from the line and upstream, follow's record
    /// restarts at `upstream`, and the switch is marked `flipped`.
    pub fn flip_authority(
        &self,
        line: &str,
        from: &str,
        to: &str,
        upstream: GitSha,
    ) -> Result<bool, GitError> {
        let mut conn = self.conn();
        let txn = conn.transaction().map_err(db_err)?;
        let moved = txn
            .execute(
                "UPDATE authority SET claim = ?3 WHERE line = ?1 AND claim = ?2",
                params![line, from, to],
            )
            .map_err(db_err)?;
        if moved != 1 {
            return Ok(false);
        }
        txn.execute("DELETE FROM exports WHERE line = ?1", params![line])
            .map_err(db_err)?;
        txn.execute(
            "INSERT INTO lines (line, upstream, behind_since_ms, diverged)
             VALUES (?1, ?2, NULL, NULL)
             ON CONFLICT (line) DO UPDATE SET upstream = ?2, behind_since_ms = NULL,
                 diverged = NULL",
            params![line, upstream.0.to_vec()],
        )
        .map_err(db_err)?;
        txn.execute(
            "UPDATE switches SET stage = 'flipped' WHERE line = ?1",
            params![line],
        )
        .map_err(db_err)?;
        txn.commit().map_err(db_err)?;
        Ok(true)
    }

    /// Export's record of `line`, if export has started it.
    pub fn export_state(&self, line: &str) -> Result<Option<ExportState>, GitError> {
        type Row = (
            i64,
            Option<Vec<u8>>,
            Option<Vec<u8>>,
            i64,
            bool,
            Option<Vec<u8>>,
        );
        let row: Option<Row> = self
            .conn()
            .query_row(
                "SELECT lsn, changeset, sha, version, diverged, stray FROM exports
                 WHERE line = ?1",
                params![line],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()
            .map_err(db_err)?;
        let Some((lsn, changeset, sha, version, diverged, stray)) = row else {
            return Ok(None);
        };
        Ok(Some(ExportState {
            lsn: lsn as u64,
            changeset: changeset.map(|b| id_col(b).map(ChangesetId)).transpose()?,
            sha: sha_col(sha)?,
            version: version as u64,
            diverged,
            stray: sha_col(stray)?,
        }))
    }

    /// Replaces export's record of `line`, in one statement: the cursor and
    /// the lease always move together.
    pub fn set_export_state(&self, line: &str, state: &ExportState) -> Result<(), GitError> {
        self.conn()
            .execute(
                "INSERT INTO exports (line, lsn, changeset, sha, version, diverged, stray)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (line) DO UPDATE SET lsn = ?2, changeset = ?3, sha = ?4,
                     version = ?5, diverged = ?6, stray = ?7",
                params![
                    line,
                    state.lsn as i64,
                    state.changeset.map(|c| c.0.0.to_vec()),
                    state.sha.map(|s| s.0.to_vec()),
                    state.version as i64,
                    state.diverged,
                    state.stray.map(|s| s.0.to_vec()),
                ],
            )
            .map(drop)
            .map_err(db_err)
    }

    /// The import record of GitHub PR `number`, if there is one.
    pub fn pr_import(&self, number: u64) -> Result<Option<PrImport>, GitError> {
        let mut found = self.pr_imports_where("WHERE number = ?1", Some(number))?;
        Ok(found.pop())
    }

    /// The PR imports that are not final, by PR number.
    pub fn open_pr_imports(&self) -> Result<Vec<PrImport>, GitError> {
        self.pr_imports_where("WHERE state IN ('held', 'landed')", None)
    }

    fn pr_imports_where(
        &self,
        filter: &str,
        number: Option<u64>,
    ) -> Result<Vec<PrImport>, GitError> {
        type Row = (
            i64,
            Vec<u8>,
            String,
            Vec<u8>,
            String,
            String,
            Option<Vec<u8>>,
        );
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT number, head_sha, workspace, head, state, detail, landed
                 FROM pr_imports {filter} ORDER BY number"
            ))
            .map_err(db_err)?;
        let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Row> {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        };
        let rows = match number {
            Some(n) => stmt.query_map(params![n as i64], map),
            None => stmt.query_map([], map),
        }
        .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (number, head_sha, workspace, head, state, detail, landed) = row.map_err(db_err)?;
            let state = ImportState::parse(&state)
                .ok_or_else(|| GitError::Map("mirror db: malformed PR import state".to_owned()))?;
            out.push(PrImport {
                number: number as u64,
                head_sha: sha_col(Some(head_sha))?.expect("present"),
                workspace,
                head: id_col(head).map(ChangesetId)?,
                state,
                detail,
                landed: landed.map(|b| id_col(b).map(ChangesetId)).transpose()?,
            });
        }
        Ok(out)
    }

    /// Writes the import record of a PR, replacing any earlier one.
    pub fn put_pr_import(&self, rec: &PrImport) -> Result<(), GitError> {
        self.conn()
            .execute(
                "INSERT INTO pr_imports (number, head_sha, workspace, head, state, detail, landed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (number) DO UPDATE SET head_sha = ?2, workspace = ?3, head = ?4,
                     state = ?5, detail = ?6, landed = ?7",
                params![
                    rec.number as i64,
                    &rec.head_sha.0[..],
                    rec.workspace,
                    &rec.head.0.0[..],
                    rec.state.as_str(),
                    rec.detail,
                    rec.landed.map(|c| c.0.0.to_vec()),
                ],
            )
            .map(drop)
            .map_err(db_err)
    }

    /// Batch mode's record of the integration line `line`, once started.
    pub fn batch_line(&self, line: &str) -> Result<Option<BatchLine>, GitError> {
        let row = self
            .conn()
            .query_row(
                "SELECT synced, sync_from, start, start_base, sync_batch FROM batch_lines
                 WHERE line = ?1",
                params![line],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                        r.get::<_, Option<Vec<u8>>>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(db_err)?;
        row.map(|(synced, sync_from, start, start_base, sync_batch)| {
            Ok(BatchLine {
                synced: id_col(synced).map(ChangesetId)?,
                sync_from: id_col(sync_from).map(ChangesetId)?,
                start: id_col(start).map(ChangesetId)?,
                // A row from before the column: unknown, so no cut needs it.
                start_base: start_base.map(id_col).transpose()?.map(ChangesetId),
                sync_batch: sync_batch.map(|n| n as u64),
            })
        })
        .transpose()
    }

    /// Records that `land` was dropped from `line`'s batches by `revert`,
    /// and why.
    pub fn put_batch_drop(
        &self,
        line: &str,
        land: ChangesetId,
        revert: ChangesetId,
        reason: &str,
    ) -> Result<(), GitError> {
        self.conn()
            .execute(
                "INSERT INTO batch_drops (line, land, revert, reason) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (line, land) DO UPDATE SET revert = ?3, reason = ?4",
                params![line, &land.0.0[..], &revert.0.0[..], reason],
            )
            .map(drop)
            .map_err(db_err)
    }

    /// Why `land` was dropped from `line`'s batches, if the mirror dropped
    /// it. One keyed read.
    pub fn batch_drop_reason(
        &self,
        line: &str,
        land: ChangesetId,
    ) -> Result<Option<String>, GitError> {
        self.conn()
            .query_row(
                "SELECT reason FROM batch_drops WHERE line = ?1 AND land = ?2",
                params![line, &land.0.0[..]],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(db_err)
    }

    /// Replaces batch mode's record of `line`.
    pub fn set_batch_line(&self, line: &str, state: &BatchLine) -> Result<(), GitError> {
        put_batch_line(&self.conn(), line, state)
    }

    /// The number the next batch gets: one past the highest so far, across
    /// every line, so branch names never repeat.
    pub fn next_batch_number(&self) -> Result<u64, GitError> {
        self.conn()
            .query_row("SELECT coalesce(max(number), 0) FROM batches", [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|n| n as u64 + 1)
            .map_err(db_err)
    }

    /// Records a new batch, in one transaction that refuses it when its
    /// number is taken or its line already has a batch that is not final:
    /// two runs never both open a batch for one line.
    /// `commits` is the batch's per-land chain (D26), oldest first, written
    /// in the same transaction; empty for a squashed batch.
    pub fn insert_batch(
        &self,
        rec: &BatchRecord,
        commits: &[BatchCommit],
    ) -> Result<bool, GitError> {
        let mut conn = self.conn();
        let txn = conn.transaction().map_err(db_err)?;
        let busy: i64 = txn
            .query_row(
                "SELECT count(*) FROM batches
                 WHERE number = ?1 OR (line = ?2 AND state NOT IN ('merged', 'rejected'))",
                params![rec.number as i64, rec.line],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        if busy > 0 {
            return Ok(false);
        }
        write_batch(&txn, rec)?;
        for c in commits {
            txn.execute(
                "INSERT INTO batch_commits (batch, seq, land, sha, merged_sha)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    rec.number as i64,
                    c.seq as i64,
                    &c.land.0.0[..],
                    &c.sha.0[..],
                    c.merged.map(|s| s.0.to_vec()),
                ],
            )
            .map_err(db_err)?;
        }
        txn.commit().map_err(db_err)?;
        Ok(true)
    }

    /// Batch `number`'s per-land commits, oldest first (D26). Empty for a
    /// squashed batch. At most one batch's commits (bounded by the cap).
    pub fn batch_commits(&self, number: u64) -> Result<Vec<BatchCommit>, GitError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT seq, land, sha, merged_sha FROM batch_commits
                 WHERE batch = ?1 ORDER BY seq",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![number as i64], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, Option<Vec<u8>>>(3)?,
                ))
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, land, sha, merged) = row.map_err(db_err)?;
            out.push(BatchCommit {
                seq: seq as u32,
                land: ChangesetId(id_col(land)?),
                sha: sha_col(Some(sha))?
                    .ok_or_else(|| GitError::Map("mirror db: malformed batch commit".into()))?,
                merged: sha_col(merged)?,
            });
        }
        Ok(out)
    }

    /// Replaces the record of batch `rec.number`.
    pub fn put_batch(&self, rec: &BatchRecord) -> Result<(), GitError> {
        write_batch(&self.conn(), rec)
    }

    /// Extends batch `rec` and records its new per-land commits atomically.
    pub fn extend_batch(&self, rec: &BatchRecord, commits: &[BatchCommit]) -> Result<(), GitError> {
        let mut conn = self.conn();
        let txn = conn.transaction().map_err(db_err)?;
        write_batch(&txn, rec)?;
        for c in commits {
            txn.execute(
                "INSERT INTO batch_commits (batch, seq, land, sha, merged_sha)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    rec.number as i64,
                    c.seq as i64,
                    &c.land.0.0[..],
                    &c.sha.0[..],
                    c.merged.map(|s| s.0.to_vec()),
                ],
            )
            .map_err(db_err)?;
        }
        txn.commit().map_err(db_err)
    }

    /// Records batch `rec` as merged, the line state the next batch
    /// starts from, and where each of `commits` merged, in one transaction.
    pub fn settle_batch(
        &self,
        rec: &BatchRecord,
        line: &str,
        state: &BatchLine,
        commits: &[BatchCommit],
    ) -> Result<(), GitError> {
        let mut conn = self.conn();
        let txn = conn.transaction().map_err(db_err)?;
        write_batch(&txn, rec)?;
        put_batch_line(&txn, line, state)?;
        for c in commits {
            txn.execute(
                "UPDATE batch_commits SET merged_sha = ?3 WHERE batch = ?1 AND seq = ?2",
                params![
                    rec.number as i64,
                    c.seq as i64,
                    c.merged.map(|s| s.0.to_vec())
                ],
            )
            .map_err(db_err)?;
        }
        txn.commit().map_err(db_err)
    }

    /// The newest batch of `line`, if it has one. One indexed read.
    pub fn last_batch(&self, line: &str) -> Result<Option<BatchRecord>, GitError> {
        let mut found = self.batches_where("WHERE line = ?1 ORDER BY number DESC LIMIT 1", line)?;
        Ok(found.pop())
    }

    /// Batch `number`, if there is one.
    pub fn batch(&self, number: u64) -> Result<Option<BatchRecord>, GitError> {
        let mut found = self.batches_where("WHERE number = ?1", &number.to_string())?;
        Ok(found.pop())
    }

    fn batches_where(&self, filter: &str, arg: &str) -> Result<Vec<BatchRecord>, GitError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT number, line, base_branch, base, base_sha, start, tip, lands,
                        initial_lands, created_ms, branch, title, body, squash_sha, pr_number,
                        pr_url, state, detail, merge_sha, merged
                 FROM batches {filter}"
            ))
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![arg], |r| {
                Ok(BatchRow {
                    number: r.get(0)?,
                    line: r.get(1)?,
                    base_branch: r.get(2)?,
                    base: r.get(3)?,
                    base_sha: r.get(4)?,
                    start: r.get(5)?,
                    tip: r.get(6)?,
                    lands: r.get(7)?,
                    initial_lands: r.get(8)?,
                    created_ms: r.get(9)?,
                    branch: r.get(10)?,
                    title: r.get(11)?,
                    body: r.get(12)?,
                    squash_sha: r.get(13)?,
                    pr_number: r.get(14)?,
                    pr_url: r.get(15)?,
                    state: r.get(16)?,
                    detail: r.get(17)?,
                    merge_sha: r.get(18)?,
                    merged: r.get(19)?,
                })
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(db_err)?.into_record()?);
        }
        Ok(out)
    }

    /// Replaces follow's record of `line`.
    pub fn set_line(&self, line: &str, state: &LineState) -> Result<(), GitError> {
        self.conn()
            .execute(
                "INSERT INTO lines (line, upstream, behind_since_ms, diverged)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (line) DO UPDATE SET upstream = ?2, behind_since_ms = ?3,
                     diverged = ?4",
                params![
                    line,
                    state.upstream.map(|s| s.0.to_vec()),
                    state.behind_since_ms,
                    state.diverged.map(|s| s.0.to_vec()),
                ],
            )
            .map(drop)
            .map_err(db_err)
    }
}

impl MirrorMap for MirrorDb {
    fn sha_of(&self, id: ChangesetId) -> Result<Option<GitSha>, GitError> {
        let sha = self
            .conn()
            .query_row(
                "SELECT sha FROM commits WHERE changeset = ?1",
                params![&id.0.0[..]],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(db_err)?;
        sha_col(sha)
    }

    fn changeset_of(&self, sha: GitSha) -> Result<Option<ChangesetId>, GitError> {
        let id = self
            .conn()
            .query_row(
                "SELECT changeset FROM commits WHERE sha = ?1",
                params![&sha.0[..]],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(db_err)?;
        id.map(|b| id_col(b).map(ChangesetId)).transpose()
    }

    fn put_imported(&self, id: ChangesetId, commit: ImportedCommit) -> Result<(), GitError> {
        self.conn()
            .execute(
                "INSERT INTO commits (changeset, sha, headers, message, raw_trees)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (changeset) DO UPDATE SET sha = ?2, headers = ?3, message = ?4,
                     raw_trees = ?5",
                params![
                    &id.0.0[..],
                    &commit.sha.0[..],
                    commit.headers,
                    commit.message,
                    encode_raw_trees(&commit.raw_trees),
                ],
            )
            .map(drop)
            .map_err(db_err)
    }

    fn imported(&self, id: ChangesetId) -> Result<Option<ImportedCommit>, GitError> {
        type Row = (Vec<u8>, Vec<u8>, Option<Vec<u8>>, Vec<u8>);
        let row: Option<Row> = self
            .conn()
            .query_row(
                "SELECT sha, headers, message, raw_trees FROM commits WHERE changeset = ?1",
                params![&id.0.0[..]],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(db_err)?;
        let Some((sha, headers, message, raw_trees)) = row else {
            return Ok(None);
        };
        Ok(Some(ImportedCommit {
            sha: sha_col(Some(sha))?.expect("present"),
            headers,
            message,
            raw_trees: decode_raw_trees(&raw_trees)?,
        }))
    }

    fn put_tag(&self, name: &str, tag: ImportedTag) -> Result<(), GitError> {
        self.conn()
            .execute(
                "INSERT INTO tags (name, sha, objects) VALUES (?1, ?2, ?3)
                 ON CONFLICT (name) DO UPDATE SET sha = ?2, objects = ?3",
                params![name, &tag.commit.0[..], encode_tag_objects(&tag.objects)],
            )
            .map(drop)
            .map_err(db_err)
    }

    fn tag(&self, name: &str) -> Result<Option<ImportedTag>, GitError> {
        let row: Option<(Vec<u8>, Vec<u8>)> = self
            .conn()
            .query_row(
                "SELECT sha, objects FROM tags WHERE name = ?1",
                params![name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db_err)?;
        let Some((sha, objects)) = row else {
            return Ok(None);
        };
        Ok(Some(ImportedTag {
            commit: sha_col(Some(sha))?.expect("present"),
            objects: decode_tag_objects(&objects)?,
        }))
    }

    fn put_blob_shas(&self, pairs: &[(BlobId, GitSha)]) -> Result<(), GitError> {
        if pairs.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn();
        let txn = conn.transaction().map_err(db_err)?;
        {
            let mut stmt = txn
                .prepare_cached("INSERT OR REPLACE INTO blobs (blob, sha) VALUES (?1, ?2)")
                .map_err(db_err)?;
            for (id, sha) in pairs {
                stmt.execute(params![&id.0.0[..], &sha.0[..]])
                    .map_err(db_err)?;
            }
        }
        txn.commit().map_err(db_err)
    }

    fn blob_sha(&self, id: BlobId) -> Result<Option<GitSha>, GitError> {
        let sha = self
            .conn()
            .query_row(
                "SELECT sha FROM blobs WHERE blob = ?1",
                params![&id.0.0[..]],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(db_err)?;
        sha_col(sha)
    }
}

fn put_batch_line(conn: &Connection, line: &str, state: &BatchLine) -> Result<(), GitError> {
    conn.execute(
        "INSERT INTO batch_lines (line, synced, sync_from, start, start_base, sync_batch)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (line) DO UPDATE SET synced = ?2, sync_from = ?3, start = ?4,
             start_base = ?5, sync_batch = ?6",
        params![
            line,
            &state.synced.0.0[..],
            &state.sync_from.0.0[..],
            &state.start.0.0[..],
            state.start_base.as_ref().map(|b| &b.0.0[..]),
            state.sync_batch.map(|n| n as i64),
        ],
    )
    .map(drop)
    .map_err(db_err)
}

/// Adds `table.column` to a DB made before it (WI-2340). The names are
/// this file's constants, never input.
fn add_column(conn: &Connection, table: &str, column: &str, ty: &str) -> Result<(), GitError> {
    let has = conn
        .prepare(&format!(
            "SELECT 1 FROM pragma_table_info('{table}') WHERE name = '{column}'"
        ))
        .and_then(|mut q| q.exists([]))
        .map_err(db_err)?;
    if !has {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ty}"))
            .map_err(db_err)?;
    }
    Ok(())
}

fn write_batch(conn: &Connection, rec: &BatchRecord) -> Result<(), GitError> {
    conn.execute(
        "INSERT INTO batches (number, line, base_branch, base, base_sha, start, tip, lands,
             initial_lands, created_ms, branch, title, body, squash_sha, pr_number, pr_url, state,
             detail, merge_sha, merged)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
             ?18, ?19, ?20)
         ON CONFLICT (number) DO UPDATE SET line = ?2, base_branch = ?3, base = ?4,
             base_sha = ?5, start = ?6, tip = ?7, lands = ?8, initial_lands = ?9,
             created_ms = ?10, branch = ?11, title = ?12, body = ?13, squash_sha = ?14,
             pr_number = ?15, pr_url = ?16, state = ?17, detail = ?18, merge_sha = ?19,
             merged = ?20",
        params![
            rec.number as i64,
            rec.line,
            rec.base_branch,
            &rec.base.0.0[..],
            &rec.base_sha.0[..],
            &rec.start.0.0[..],
            &rec.tip.0.0[..],
            rec.lands as i64,
            rec.initial_lands as i64,
            rec.created_ms,
            rec.branch,
            rec.title,
            rec.body,
            &rec.squash_sha.0[..],
            rec.pr_number.map(|n| n as i64),
            rec.pr_url,
            rec.state.as_str(),
            rec.detail,
            rec.merge_sha.map(|s| s.0.to_vec()),
            rec.merged.map(|c| c.0.0.to_vec()),
        ],
    )
    .map(drop)
    .map_err(db_err)
}

/// A `batches` row as stored.
struct BatchRow {
    number: i64,
    line: String,
    base_branch: String,
    base: Vec<u8>,
    base_sha: Vec<u8>,
    start: Vec<u8>,
    tip: Vec<u8>,
    lands: i64,
    initial_lands: i64,
    created_ms: i64,
    branch: String,
    title: String,
    body: String,
    squash_sha: Vec<u8>,
    pr_number: Option<i64>,
    pr_url: Option<String>,
    state: String,
    detail: String,
    merge_sha: Option<Vec<u8>>,
    merged: Option<Vec<u8>>,
}

impl BatchRow {
    fn into_record(self) -> Result<BatchRecord, GitError> {
        let bad = |what: &str| GitError::Map(format!("mirror db: malformed batch row ({what})"));
        let id = |b: Vec<u8>| id_col(b).map(ChangesetId);
        Ok(BatchRecord {
            number: self.number as u64,
            line: self.line,
            base_branch: self.base_branch,
            base: id(self.base)?,
            base_sha: sha_col(Some(self.base_sha))?.ok_or_else(|| bad("base_sha"))?,
            start: id(self.start)?,
            tip: id(self.tip)?,
            lands: self.lands as u64,
            initial_lands: self.initial_lands as u64,
            created_ms: self.created_ms,
            branch: self.branch,
            title: self.title,
            body: self.body,
            squash_sha: sha_col(Some(self.squash_sha))?.ok_or_else(|| bad("squash_sha"))?,
            pr_number: self.pr_number.map(|n| n as u64),
            pr_url: self.pr_url,
            state: BatchState::parse(&self.state).ok_or_else(|| bad("state"))?,
            detail: self.detail,
            merge_sha: sha_col(self.merge_sha)?,
            merged: self.merged.map(id).transpose()?,
        })
    }
}

/// Raw tag objects as `count:u32` then, per object, `len:u32` + bytes.
/// Local to the mirror DB; not a graft durable format.
fn encode_tag_objects(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    for o in objects {
        out.extend_from_slice(&(o.len() as u32).to_be_bytes());
        out.extend_from_slice(o);
    }
    out
}

fn decode_tag_objects(mut bytes: &[u8]) -> Result<Vec<Vec<u8>>, GitError> {
    fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Result<&'a [u8], GitError> {
        if bytes.len() < n {
            return Err(GitError::Map("mirror db: malformed tag objects".to_owned()));
        }
        let (head, rest) = bytes.split_at(n);
        *bytes = rest;
        Ok(head)
    }
    fn len(bytes: &mut &[u8]) -> Result<usize, GitError> {
        let b = take(bytes, 4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
    }
    let n = len(&mut bytes)?;
    let mut out = Vec::with_capacity(n.min(16));
    for _ in 0..n {
        let l = len(&mut bytes)?;
        out.push(take(&mut bytes, l)?.to_vec());
    }
    if !bytes.is_empty() {
        return Err(GitError::Map("mirror db: malformed tag objects".to_owned()));
    }
    Ok(out)
}

/// Raw trees as `count:u32` then, per tree, an optional directory and an
/// optional tree object, each as `present:u8` and `len:u32` + bytes. Local
/// to the mirror DB; not a graft durable format.
fn encode_raw_trees(trees: &[RawTree]) -> Vec<u8> {
    fn opt(out: &mut Vec<u8>, bytes: Option<&[u8]>) {
        match bytes {
            None => out.push(0),
            Some(b) => {
                out.push(1);
                out.extend_from_slice(&(b.len() as u32).to_be_bytes());
                out.extend_from_slice(b);
            }
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(&(trees.len() as u32).to_be_bytes());
    for t in trees {
        opt(&mut out, t.dir.as_ref().map(|d| d.as_str().as_bytes()));
        opt(&mut out, t.tree.as_deref());
    }
    out
}

fn decode_raw_trees(mut bytes: &[u8]) -> Result<Vec<RawTree>, GitError> {
    fn bad() -> GitError {
        GitError::Map("mirror db: malformed raw trees".to_owned())
    }
    fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Result<&'a [u8], GitError> {
        if bytes.len() < n {
            return Err(bad());
        }
        let (head, rest) = bytes.split_at(n);
        *bytes = rest;
        Ok(head)
    }
    fn u32_at(bytes: &mut &[u8]) -> Result<usize, GitError> {
        let b = take(bytes, 4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
    }
    fn opt<'a>(bytes: &mut &'a [u8]) -> Result<Option<&'a [u8]>, GitError> {
        match take(bytes, 1)?[0] {
            0 => Ok(None),
            1 => {
                let n = u32_at(bytes)?;
                take(bytes, n).map(Some)
            }
            _ => Err(bad()),
        }
    }
    let count = u32_at(&mut bytes)?;
    let mut out = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let dir = opt(&mut bytes)?
            .map(|d| RepoPath::from_bytes(d).map_err(|_| bad()))
            .transpose()?;
        let tree = opt(&mut bytes)?.map(<[u8]>::to_vec);
        out.push(RawTree { dir, tree });
    }
    if !bytes.is_empty() {
        return Err(bad());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_commits_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db = MirrorDb::open(&dir.path().join("mirror.sqlite")).unwrap();
        let id = ChangesetId(ObjectId([7; 32]));
        let commit = ImportedCommit {
            sha: GitSha([9; 20]),
            headers: b"author A <a@b> 1 +0000\n\n".to_vec(),
            message: Some(vec![0xff, b'x']),
            raw_trees: vec![
                RawTree {
                    dir: None,
                    tree: Some(vec![1, 2, 3]),
                },
                RawTree {
                    dir: Some(RepoPath::new("a/b").unwrap()),
                    tree: None,
                },
            ],
        };
        db.put_imported(id, commit.clone()).unwrap();
        db.put_blob_shas(&[(BlobId(ObjectId([3; 32])), GitSha([4; 20]))])
            .unwrap();
        db.set_line(
            "main",
            &LineState {
                upstream: Some(GitSha([9; 20])),
                behind_since_ms: Some(5),
                diverged: None,
            },
        )
        .unwrap();
        drop(db);

        let db = MirrorDb::open(&dir.path().join("mirror.sqlite")).unwrap();
        assert_eq!(db.imported(id).unwrap(), Some(commit));
        assert_eq!(db.sha_of(id).unwrap(), Some(GitSha([9; 20])));
        assert_eq!(db.changeset_of(GitSha([9; 20])).unwrap(), Some(id));
        assert_eq!(db.changeset_of(GitSha([8; 20])).unwrap(), None);
        assert_eq!(
            db.blob_sha(BlobId(ObjectId([3; 32]))).unwrap(),
            Some(GitSha([4; 20]))
        );
        assert_eq!(db.line("main").unwrap().behind_since_ms, Some(5));
        assert_eq!(db.line("other").unwrap(), LineState::default());
        assert_eq!(db.commit_count().unwrap(), 1);
    }

    /// A mirror DB written before the `tags` table existed gains it on open,
    /// keeps its commits, and records tags from then on.
    #[test]
    fn a_db_without_the_tags_table_is_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirror.sqlite");
        let db = MirrorDb::open(&path).unwrap();
        let id = ChangesetId(ObjectId([7; 32]));
        let commit = ImportedCommit {
            sha: GitSha([9; 20]),
            headers: b"author A <a@b> 1 +0000\n\n".to_vec(),
            message: None,
            raw_trees: Vec::new(),
        };
        db.put_imported(id, commit.clone()).unwrap();
        db.conn().execute_batch("DROP TABLE tags").unwrap();
        drop(db);

        let db = MirrorDb::open(&path).unwrap();
        assert_eq!(db.imported(id).unwrap(), Some(commit));
        assert_eq!(db.tag("v1").unwrap(), None);
        let tag = ImportedTag {
            commit: GitSha([9; 20]),
            objects: vec![b"object outer".to_vec(), b"object inner".to_vec()],
        };
        db.put_tag("v1", tag.clone()).unwrap();
        drop(db);
        let db = MirrorDb::open(&path).unwrap();
        assert_eq!(db.tag("v1").unwrap(), Some(tag));
    }

    #[test]
    fn malformed_tag_objects_are_refused() {
        assert!(decode_tag_objects(&[0, 0, 0, 1, 0, 0, 0, 2, 1]).is_err());
        assert!(decode_tag_objects(&[0, 0, 0, 0, 9]).is_err());
        assert_eq!(
            decode_tag_objects(&[0, 0, 0, 0]).unwrap(),
            Vec::<Vec<u8>>::new()
        );
    }

    #[test]
    fn malformed_raw_trees_are_refused() {
        assert!(decode_raw_trees(&[0, 0, 0, 1, 2]).is_err());
        assert!(decode_raw_trees(&[0, 0, 0, 0, 9]).is_err());
        assert_eq!(decode_raw_trees(&[0, 0, 0, 0]).unwrap(), Vec::new());
    }
}
