//! The reader pool and the read queries.
//!
//! Each reader thread owns one `query_only` connection. In WAL mode a read
//! never waits for the writer, and it sees every commit that finished before
//! it started, so an acknowledged write is visible to the next read.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};

use graft_model::{ChangesetId, ObjectId, RepoPath};
use graft_store::{
    ChangesetRecord, DoomMark, Lsn, Protection, RecordState, RefRecord, RepoId, StoreError,
    StoreResult, TimedRow, WorkspaceRecord, page_limit,
};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::codec::{changeset_id, int, object_id, ref_kind, sql_err, state, state_code, uint};

/// A queued read: runs against a reader's connection and answers itself.
pub(crate) type ReadJob = Box<dyn FnOnce(&Connection) + Send>;

/// One reader thread's loop. Readers share the queue; it ends when every
/// sender is gone.
pub(crate) fn run(conn: Connection, jobs: Arc<Mutex<Receiver<ReadJob>>>) {
    loop {
        let job = {
            let queue = jobs.lock().unwrap_or_else(PoisonError::into_inner);
            queue.recv()
        };
        match job {
            Ok(job) => job(&conn),
            Err(_) => return,
        }
    }
}

/// Fails with `Invalid` for a repository that does not exist.
pub(crate) fn check_repo(conn: &Connection, repo: RepoId) -> StoreResult<()> {
    let exists = conn
        .prepare_cached("SELECT 1 FROM repos WHERE id = ?1")
        .and_then(|mut s| s.exists([repo.0]))
        .map_err(sql_err)?;
    if exists {
        Ok(())
    } else {
        Err(StoreError::Invalid(format!(
            "unknown repository {}",
            repo.0
        )))
    }
}

pub(crate) fn repo(conn: &Connection, name: &str) -> StoreResult<Option<RepoId>> {
    let id: Option<i64> = conn
        .prepare_cached("SELECT id FROM repos WHERE name = ?1")
        .and_then(|mut s| s.query_row([name], |r| r.get(0)).optional())
        .map_err(sql_err)?;
    id.map(|id| {
        u32::try_from(id)
            .map(RepoId)
            .map_err(|_| StoreError::Corrupt(format!("repository id {id}")))
    })
    .transpose()
}

pub(crate) fn changeset(
    conn: &Connection,
    repo: RepoId,
    id: ChangesetId,
) -> StoreResult<Option<(Vec<u8>, ChangesetRecord)>> {
    check_repo(conn, repo)?;
    type Raw = (Vec<u8>, i64, i64, i64, Option<String>, Option<Vec<u8>>);
    let raw: Option<Raw> = conn
        .prepare_cached(
            "SELECT content, lsn, received_at, state, holder, superseded_by
             FROM changesets WHERE repo = ?1 AND id = ?2",
        )
        .and_then(|mut s| {
            s.query_row(params![repo.0, &id.0.0[..]], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .optional()
        })
        .map_err(sql_err)?;
    let Some((content, lsn, received_at_ms, code, holder, superseded_by)) = raw else {
        return Ok(None);
    };
    let record = ChangesetRecord {
        lsn: uint(lsn)?,
        received_at_ms,
        state: state(code)?,
        holder,
        superseded_by: superseded_by.as_deref().map(changeset_id).transpose()?,
    };
    Ok(Some((content, record)))
}

type RawRef = (String, Vec<u8>, i64, i64);

fn raw_ref(r: &Row<'_>) -> rusqlite::Result<RawRef> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
}

fn ref_record((name, target, version, kind): RawRef) -> StoreResult<RefRecord> {
    Ok(RefRecord {
        name,
        target: changeset_id(&target)?,
        version: uint(version)?,
        kind: ref_kind(kind)?,
    })
}

/// A ref by name, without the repository check (the writer uses it too).
pub(crate) fn read_ref(conn: &Connection, repo: i64, name: &str) -> StoreResult<Option<RefRecord>> {
    conn.prepare_cached(
        "SELECT name, target, version, kind FROM refs WHERE repo = ?1 AND name = ?2",
    )
    .and_then(|mut s| s.query_row(params![repo, name], raw_ref).optional())
    .map_err(sql_err)?
    .map(ref_record)
    .transpose()
}

pub(crate) fn get_ref(
    conn: &Connection,
    repo: RepoId,
    name: &str,
) -> StoreResult<Option<RefRecord>> {
    check_repo(conn, repo)?;
    read_ref(conn, repo.0.into(), name)
}

pub(crate) fn list_refs(
    conn: &Connection,
    repo: RepoId,
    prefix: &str,
    limit: u32,
) -> StoreResult<Vec<RefRecord>> {
    check_repo(conn, repo)?;
    let mut stmt = conn
        .prepare_cached(
            "SELECT name, target, version, kind FROM refs
             WHERE repo = ?1 AND name >= ?2 ORDER BY name",
        )
        .map_err(sql_err)?;
    // Rows are stepped lazily, so the scan stops at the first name past the
    // prefix: the cost is the page, not the ref count.
    let mut rows = stmt.query(params![repo.0, prefix]).map_err(sql_err)?;
    let mut out = Vec::new();
    while out.len() < page_limit(limit) {
        let Some(row) = rows.next().map_err(sql_err)? else {
            break;
        };
        let raw = raw_ref(row).map_err(sql_err)?;
        if !raw.0.starts_with(prefix) {
            break;
        }
        out.push(ref_record(raw)?);
    }
    Ok(out)
}

type RawWorkspace = (
    String,
    String,
    Option<String>,
    String,
    Vec<u8>,
    Option<Vec<u8>>,
    i64,
);

const WORKSPACE_COLUMNS: &str = "name, owner, session, line, base, head, lease_until";

fn raw_workspace(r: &Row<'_>) -> rusqlite::Result<RawWorkspace> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
    ))
}

fn workspace_record(raw: RawWorkspace) -> StoreResult<WorkspaceRecord> {
    let (name, owner, session, line, base, head, lease_until_ms) = raw;
    Ok(WorkspaceRecord {
        name,
        owner,
        session,
        line,
        base: changeset_id(&base)?,
        head: head.as_deref().map(changeset_id).transpose()?,
        lease_until_ms,
    })
}

pub(crate) fn workspace(
    conn: &Connection,
    repo: RepoId,
    name: &str,
) -> StoreResult<Option<WorkspaceRecord>> {
    check_repo(conn, repo)?;
    conn.prepare_cached(&format!(
        "SELECT {WORKSPACE_COLUMNS} FROM workspaces WHERE repo = ?1 AND name = ?2"
    ))
    .and_then(|mut s| s.query_row(params![repo.0, name], raw_workspace).optional())
    .map_err(sql_err)?
    .map(workspace_record)
    .transpose()
}

pub(crate) fn expired_workspaces(
    conn: &Connection,
    repo: RepoId,
    before_ms: i64,
    limit: u32,
) -> StoreResult<Vec<WorkspaceRecord>> {
    check_repo(conn, repo)?;
    let limit = page_limit(limit) as i64;
    let mut stmt = conn
        .prepare_cached(&format!(
            "SELECT {WORKSPACE_COLUMNS} FROM workspaces
             WHERE repo = ?1 AND lease_until < ?2 ORDER BY lease_until, name LIMIT ?3"
        ))
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![repo.0, before_ms, limit], raw_workspace)
        .map_err(sql_err)?;
    rows.map(|r| r.map_err(sql_err).and_then(workspace_record))
        .collect()
}

pub(crate) fn list_workspaces(
    conn: &Connection,
    repo: RepoId,
    owner: &str,
    after: &str,
    limit: u32,
) -> StoreResult<Vec<WorkspaceRecord>> {
    check_repo(conn, repo)?;
    let limit = page_limit(limit) as i64;
    let mut stmt = conn
        .prepare_cached(&format!(
            "SELECT {WORKSPACE_COLUMNS} FROM workspaces
             WHERE repo = ?1 AND owner = ?2 AND name > ?3 ORDER BY name LIMIT ?4"
        ))
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![repo.0, owner, after, limit], raw_workspace)
        .map_err(sql_err)?;
    rows.map(|r| r.map_err(sql_err).and_then(workspace_record))
        .collect()
}

pub(crate) fn count_records(
    conn: &Connection,
    repo: RepoId,
    state: RecordState,
) -> StoreResult<u64> {
    check_repo(conn, repo)?;
    let n: i64 = conn
        .prepare_cached("SELECT count(*) FROM changesets WHERE repo = ?1 AND state = ?2")
        .and_then(|mut s| s.query_row(params![repo.0, state_code(state)], |r| r.get(0)))
        .map_err(sql_err)?;
    uint(n)
}

pub(crate) fn count_workspaces(
    conn: &Connection,
    repo: RepoId,
    before_ms: Option<i64>,
) -> StoreResult<u64> {
    check_repo(conn, repo)?;
    let n: i64 = conn
        .prepare_cached(
            "SELECT count(*) FROM workspaces
             WHERE repo = ?1 AND (?2 IS NULL OR lease_until < ?2)",
        )
        .and_then(|mut s| s.query_row(params![repo.0, before_ms], |r| r.get(0)))
        .map_err(sql_err)?;
    uint(n)
}

pub(crate) fn derived(
    conn: &Connection,
    repo: RepoId,
    kind: &str,
    version: u32,
    key: &[u8],
) -> StoreResult<Option<Vec<u8>>> {
    check_repo(conn, repo)?;
    conn.prepare_cached(
        "SELECT value FROM derived WHERE repo = ?1 AND kind = ?2 AND ver = ?3 AND key = ?4",
    )
    .and_then(|mut s| {
        s.query_row(params![repo.0, kind, version, key], |r| r.get(0))
            .optional()
    })
    .map_err(sql_err)
}

pub(crate) fn line_pos(
    conn: &Connection,
    repo: RepoId,
    line: &str,
    id: ChangesetId,
) -> StoreResult<Option<u64>> {
    check_repo(conn, repo)?;
    let pos: Option<i64> = conn
        .prepare_cached("SELECT pos FROM line_pos WHERE repo = ?1 AND line = ?2 AND changeset = ?3")
        .and_then(|mut s| {
            s.query_row(params![repo.0, line, &id.0.0[..]], |r| r.get(0))
                .optional()
        })
        .map_err(sql_err)?;
    pos.map(uint).transpose()
}

pub(crate) fn scan_line_path(
    conn: &Connection,
    repo: RepoId,
    line: &str,
    path: &RepoPath,
    before: u64,
    limit: u32,
) -> StoreResult<Vec<(u64, ChangesetId)>> {
    check_repo(conn, repo)?;
    // Positions above the integer range are all "before" every stored one.
    let before = int(before).unwrap_or(i64::MAX);
    let limit = page_limit(limit) as i64;
    let mut stmt = conn
        .prepare_cached(
            "SELECT pos, changeset FROM line_paths
             WHERE repo = ?1 AND line = ?2 AND path = ?3 AND pos < ?4
             ORDER BY pos DESC, changeset DESC LIMIT ?5",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![repo.0, line, path.as_str(), before, limit], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
        })
        .map_err(sql_err)?;
    rows.map(|r| {
        let (pos, id) = r.map_err(sql_err)?;
        Ok((uint(pos)?, changeset_id(&id)?))
    })
    .collect()
}

pub(crate) fn holders(conn: &Connection, repo: RepoId, object: ObjectId) -> StoreResult<u32> {
    check_repo(conn, repo)?;
    let count: i64 = conn
        .prepare_cached("SELECT count(*) FROM holders WHERE repo = ?1 AND object = ?2")
        .and_then(|mut s| s.query_row(params![repo.0, &object.0[..]], |r| r.get(0)))
        .map_err(sql_err)?;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

pub(crate) fn land(
    conn: &Connection,
    repo: RepoId,
    draft: ChangesetId,
    line: &str,
) -> StoreResult<Option<ChangesetId>> {
    check_repo(conn, repo)?;
    let landed: Option<Vec<u8>> = conn
        .prepare_cached("SELECT landed FROM lands WHERE repo = ?1 AND draft = ?2 AND line = ?3")
        .and_then(|mut s| {
            s.query_row(params![repo.0, &draft.0.0[..], line], |r| r.get(0))
                .optional()
        })
        .map_err(sql_err)?;
    landed.as_deref().map(changeset_id).transpose()
}

pub(crate) fn journal(
    conn: &Connection,
    repo: RepoId,
    from: Lsn,
    limit: u32,
) -> StoreResult<Vec<(Lsn, String, Vec<u8>)>> {
    check_repo(conn, repo)?;
    let from = int(from).unwrap_or(i64::MAX);
    let limit = page_limit(limit) as i64;
    let mut stmt = conn
        .prepare_cached(
            "SELECT lsn, kind, body FROM journal
             WHERE repo = ?1 AND lsn >= ?2 ORDER BY lsn LIMIT ?3",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![repo.0, from, limit], |r| {
            Ok((r.get::<_, i64>(0)?, r.get(1)?, r.get(2)?))
        })
        .map_err(sql_err)?;
    rows.map(|r| {
        let (lsn, kind, body) = r.map_err(sql_err)?;
        Ok((uint(lsn)?, kind, body))
    })
    .collect()
}

pub(crate) fn list_records(
    conn: &Connection,
    repo: RepoId,
    state: RecordState,
    after: Option<ChangesetId>,
    limit: u32,
) -> StoreResult<Vec<(ChangesetId, ChangesetRecord)>> {
    check_repo(conn, repo)?;
    let after = after.map(|id| id.0.0.to_vec());
    let limit = page_limit(limit) as i64;
    let mut stmt = conn
        .prepare_cached(
            "SELECT id, lsn, received_at, holder, superseded_by FROM changesets
             WHERE repo = ?1 AND state = ?2 AND (?3 IS NULL OR id > ?3)
             ORDER BY id LIMIT ?4",
        )
        .map_err(sql_err)?;
    type Raw = (Vec<u8>, i64, i64, Option<String>, Option<Vec<u8>>);
    let rows = stmt
        .query_map(
            params![repo.0, state_code(state), after, limit],
            |r| -> rusqlite::Result<Raw> {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            },
        )
        .map_err(sql_err)?;
    rows.map(|row| {
        let (id, lsn, received_at_ms, holder, superseded_by) = row.map_err(sql_err)?;
        let record = ChangesetRecord {
            lsn: uint(lsn)?,
            received_at_ms,
            state,
            holder,
            superseded_by: superseded_by.as_deref().map(changeset_id).transpose()?,
        };
        Ok((changeset_id(&id)?, record))
    })
    .collect()
}

pub(crate) fn holder_rows(
    conn: &Connection,
    repo: RepoId,
    object: ObjectId,
    after: Option<String>,
    limit: u32,
) -> StoreResult<Vec<String>> {
    check_repo(conn, repo)?;
    let limit = page_limit(limit) as i64;
    let mut stmt = conn
        .prepare_cached(
            "SELECT holder FROM holders
             WHERE repo = ?1 AND object = ?2 AND (?3 IS NULL OR holder > ?3)
             ORDER BY holder LIMIT ?4",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![repo.0, &object.0[..], after, limit], |r| {
            r.get::<_, String>(0)
        })
        .map_err(sql_err)?;
    rows.map(|r| r.map_err(sql_err)).collect()
}

pub(crate) fn list_all_workspaces(
    conn: &Connection,
    repo: RepoId,
    after: &str,
    limit: u32,
) -> StoreResult<Vec<WorkspaceRecord>> {
    check_repo(conn, repo)?;
    let limit = page_limit(limit) as i64;
    let mut stmt = conn
        .prepare_cached(&format!(
            "SELECT {WORKSPACE_COLUMNS} FROM workspaces
             WHERE repo = ?1 AND name > ?2 ORDER BY name LIMIT ?3"
        ))
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![repo.0, after, limit], raw_workspace)
        .map_err(sql_err)?;
    rows.map(|r| r.map_err(sql_err).and_then(workspace_record))
        .collect()
}

/// A record's protection, without the repository check (the writer uses it
/// too).
pub(crate) fn read_protection(
    conn: &Connection,
    repo: i64,
    id: ChangesetId,
) -> StoreResult<Option<Protection>> {
    let raw: Option<(i64, Option<i64>)> = conn
        .prepare_cached("SELECT pins, expires_at FROM changesets WHERE repo = ?1 AND id = ?2")
        .and_then(|mut s| {
            s.query_row(params![repo, &id.0.0[..]], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()
        })
        .map_err(sql_err)?;
    raw.map(|(pins, expires_at_ms)| {
        Ok(Protection {
            pins: u32::try_from(pins).map_err(|_| StoreError::Corrupt(format!("pins {pins}")))?,
            expires_at_ms,
        })
    })
    .transpose()
}

/// An object's doomed mark, without the repository check.
pub(crate) fn doom_mark(
    conn: &Connection,
    repo: i64,
    object: ObjectId,
) -> StoreResult<Option<DoomMark>> {
    conn.prepare_cached("SELECT since, deleting FROM doomed WHERE repo = ?1 AND object = ?2")
        .and_then(|mut s| {
            s.query_row(params![repo, &object.0[..]], |r| {
                Ok(DoomMark {
                    since_ms: r.get(0)?,
                    deleting: r.get(1)?,
                })
            })
            .optional()
        })
        .map_err(sql_err)
}

pub(crate) fn protection(
    conn: &Connection,
    repo: RepoId,
    id: ChangesetId,
) -> StoreResult<Option<Protection>> {
    check_repo(conn, repo)?;
    read_protection(conn, repo.0.into(), id)
}

pub(crate) fn expired_records(
    conn: &Connection,
    repo: RepoId,
    now_ms: i64,
    after: Option<(i64, ChangesetId)>,
    limit: u32,
) -> StoreResult<Vec<(i64, ChangesetId)>> {
    check_repo(conn, repo)?;
    // With no cursor, start below every row: (i64::MIN, empty id).
    let (at, id) = after.map_or((i64::MIN, Vec::new()), |(at, id)| (at, id.0.0.to_vec()));
    let limit = page_limit(limit) as i64;
    let mut stmt = conn
        .prepare_cached(
            "SELECT expires_at, id FROM changesets
             WHERE repo = ?1 AND pins = 0 AND expires_at IS NOT NULL
               AND expires_at <= ?2 AND (expires_at, id) > (?3, ?4)
             ORDER BY expires_at, id LIMIT ?5",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![repo.0, now_ms, at, id, limit], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
        })
        .map_err(sql_err)?;
    rows.map(|r| {
        let (at, id) = r.map_err(sql_err)?;
        Ok((at, changeset_id(&id)?))
    })
    .collect()
}

pub(crate) fn held_objects(
    conn: &Connection,
    repo: RepoId,
    holder: &str,
    after: Option<ObjectId>,
    limit: u32,
) -> StoreResult<Vec<ObjectId>> {
    check_repo(conn, repo)?;
    let after = after.map_or(Vec::new(), |o| o.0.to_vec());
    let limit = page_limit(limit) as i64;
    let mut stmt = conn
        .prepare_cached(
            "SELECT object FROM holders
             WHERE repo = ?1 AND holder = ?2 AND object > ?3
             ORDER BY object LIMIT ?4",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![repo.0, holder, after, limit], |r| {
            r.get::<_, Vec<u8>>(0)
        })
        .map_err(sql_err)?;
    rows.map(|r| object_id(&r.map_err(sql_err)?)).collect()
}

pub(crate) fn expired_rows(
    conn: &Connection,
    repo: RepoId,
    now_ms: i64,
    after: Option<TimedRow>,
    limit: u32,
) -> StoreResult<Vec<TimedRow>> {
    check_repo(conn, repo)?;
    let (at, object, holder) = after.map_or((i64::MIN, Vec::new(), String::new()), |row| {
        (row.expires_at_ms, row.object.0.to_vec(), row.holder)
    });
    let limit = page_limit(limit) as i64;
    let mut stmt = conn
        .prepare_cached(
            "SELECT expires, object, holder FROM holders
             WHERE repo = ?1 AND expires IS NOT NULL AND expires <= ?2
               AND (expires, object, holder) > (?3, ?4, ?5)
             ORDER BY expires, object, holder LIMIT ?6",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![repo.0, now_ms, at, object, holder, limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(sql_err)?;
    rows.map(|r| {
        let (expires_at_ms, object, holder) = r.map_err(sql_err)?;
        Ok(TimedRow {
            expires_at_ms,
            object: object_id(&object)?,
            holder,
        })
    })
    .collect()
}

pub(crate) fn doom_marks(
    conn: &Connection,
    repo: RepoId,
    ids: &[ObjectId],
) -> StoreResult<Vec<(ObjectId, DoomMark)>> {
    check_repo(conn, repo)?;
    let mut out = Vec::new();
    for id in ids {
        if let Some(mark) = doom_mark(conn, repo.0.into(), *id)? {
            out.push((*id, mark));
        }
    }
    Ok(out)
}

pub(crate) fn old_doomed(
    conn: &Connection,
    repo: RepoId,
    until_ms: i64,
    after: Option<(i64, ObjectId)>,
    limit: u32,
) -> StoreResult<Vec<(ObjectId, DoomMark)>> {
    check_repo(conn, repo)?;
    let (at, object) = after.map_or((i64::MIN, Vec::new()), |(at, o)| (at, o.0.to_vec()));
    let limit = page_limit(limit) as i64;
    let mut stmt = conn
        .prepare_cached(
            "SELECT object, since, deleting FROM doomed
             WHERE repo = ?1 AND since <= ?2 AND (since, object) > (?3, ?4)
             ORDER BY since, object LIMIT ?5",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![repo.0, until_ms, at, object, limit], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get(1)?, r.get(2)?))
        })
        .map_err(sql_err)?;
    rows.map(|r| {
        let (object, since_ms, deleting) = r.map_err(sql_err)?;
        Ok((object_id(&object)?, DoomMark { since_ms, deleting }))
    })
    .collect()
}

pub(crate) fn lapsed_workspaces(
    conn: &Connection,
    repo: RepoId,
    before_ms: i64,
    after: Option<(i64, String)>,
    limit: u32,
) -> StoreResult<Vec<WorkspaceRecord>> {
    check_repo(conn, repo)?;
    let (at, name) = after.unwrap_or((i64::MIN, String::new()));
    let limit = page_limit(limit) as i64;
    let mut stmt = conn
        .prepare_cached(&format!(
            "SELECT {WORKSPACE_COLUMNS} FROM workspaces
             WHERE repo = ?1 AND lease_until < ?2 AND (lease_until, name) > (?3, ?4)
             ORDER BY lease_until, name LIMIT ?5"
        ))
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![repo.0, before_ms, at, name, limit], raw_workspace)
        .map_err(sql_err)?;
    rows.map(|r| r.map_err(sql_err).and_then(workspace_record))
        .collect()
}

pub(crate) fn gc_protection(conn: &Connection, repo: RepoId) -> StoreResult<bool> {
    check_repo(conn, repo)?;
    conn.prepare_cached("SELECT gc_protection FROM repos WHERE id = ?1")
        .and_then(|mut s| s.query_row([repo.0], |r| r.get(0)))
        .map_err(sql_err)
}
