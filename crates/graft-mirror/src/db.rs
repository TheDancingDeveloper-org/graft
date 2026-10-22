//! The mirror DB (`docs/design.md` §11.1 step 5, §11.3): a SQLite file the
//! mirror owns, outside the graft server. It holds the mirror map (graft ids
//! ↔ git shas, raw commit headers, raw trees, blob shas) and, per followed
//! line, how far follow has got.
//!
//! Every write is one statement or one transaction, so a crash leaves either
//! the old row or the new one. The import records a commit only after the
//! server has accepted its changeset, so a restart re-walks at most the batch
//! that was in flight, and the server answers those retries idempotently.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use graft_git::map::{ImportedCommit, MirrorMap, RawTree};
use graft_git::{GitError, GitSha};
use graft_model::{BlobId, ChangesetId, ObjectId, RepoPath};
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS commits (
    changeset BLOB PRIMARY KEY,
    sha BLOB NOT NULL UNIQUE,
    headers BLOB NOT NULL,
    message BLOB,
    raw_trees BLOB NOT NULL
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

/// The mirror DB. One per mirrored graft repository.
pub struct MirrorDb {
    conn: Mutex<Connection>,
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
        Ok(MirrorDb {
            conn: Mutex::new(conn),
        })
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

    #[test]
    fn malformed_raw_trees_are_refused() {
        assert!(decode_raw_trees(&[0, 0, 0, 1, 2]).is_err());
        assert!(decode_raw_trees(&[0, 0, 0, 0, 9]).is_err());
        assert_eq!(decode_raw_trees(&[0, 0, 0, 0]).unwrap(), Vec::new());
    }
}
