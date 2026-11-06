//! The `mirror_requests` table and the per-line request slots (D41 §3.5).
//!
//! Change ids (a batch base, its excludes and lands) are stored as
//! concatenated 32-byte blobs.

use graft_model::ChangesetId;
use graft_store::{
    BatchState, LineSlot, MirrorRequestKind, MirrorRequestQuery, MirrorRequestRecord,
    MirrorRequestResult, MirrorRequestState, StoreError, StoreResult, page_limit,
};
use rusqlite::{Connection, OptionalExtension as _, Row, params, params_from_iter, types::Value};

use crate::codec::{changeset_id, int, sql_err, uint};

const COLUMNS: &str = "id, line, kind, client_request_id, base, excludes, upstream, state,
     batch_state, claimed_by_epoch, claimed_at, deadline, pr_number, pr_url, branch, lands,
     error_code, requested_by, created_at, updated_at, version";

fn ids_blob(ids: &[ChangesetId]) -> Vec<u8> {
    ids.iter().flat_map(|c| c.0.0).collect()
}

fn ids_of(blob: &[u8]) -> StoreResult<Vec<ChangesetId>> {
    if !blob.len().is_multiple_of(32) {
        return Err(StoreError::Corrupt("mirror request id list".into()));
    }
    blob.chunks(32).map(changeset_id).collect()
}

fn corrupt(what: &str, v: &str) -> StoreError {
    StoreError::Corrupt(format!("mirror request {what} {v:?}"))
}

fn record(r: &Row<'_>) -> StoreResult<MirrorRequestRecord> {
    let g = |e: rusqlite::Error| sql_err(e);
    let kind: String = r.get(2).map_err(g)?;
    let state: String = r.get(7).map_err(g)?;
    let batch: Option<String> = r.get(8).map_err(g)?;
    let base: Option<Vec<u8>> = r.get(4).map_err(g)?;
    let excludes: Vec<u8> = r.get(5).map_err(g)?;
    let lands: Vec<u8> = r.get(15).map_err(g)?;
    let epoch: Option<i64> = r.get(9).map_err(g)?;
    let pr: Option<i64> = r.get(12).map_err(g)?;
    Ok(MirrorRequestRecord {
        id: uint(r.get(0).map_err(g)?)?,
        line: r.get(1).map_err(g)?,
        kind: MirrorRequestKind::parse(&kind).ok_or_else(|| corrupt("kind", &kind))?,
        client_request_id: r.get(3).map_err(g)?,
        base: base.as_deref().map(changeset_id).transpose()?,
        excludes: ids_of(&excludes)?,
        upstream: r.get(6).map_err(g)?,
        state: MirrorRequestState::parse(&state).ok_or_else(|| corrupt("state", &state))?,
        batch: batch
            .map(|b| BatchState::parse(&b).ok_or_else(|| corrupt("batch state", &b)))
            .transpose()?,
        claimed_by_epoch: epoch.map(uint).transpose()?,
        claimed_at_ms: r.get(10).map_err(g)?,
        deadline_ms: r.get(11).map_err(g)?,
        result: MirrorRequestResult {
            pr_number: pr.map(uint).transpose()?,
            pr_url: r.get(13).map_err(g)?,
            branch: r.get(14).map_err(g)?,
            lands: ids_of(&lands)?,
            error_code: r.get(16).map_err(g)?,
        },
        requested_by: r.get(17).map_err(g)?,
        created_at_ms: r.get(18).map_err(g)?,
        updated_at_ms: r.get(19).map_err(g)?,
        version: uint(r.get(20).map_err(g)?)?,
    })
}

fn one(
    conn: &Connection,
    sql: &str,
    args: impl rusqlite::Params,
) -> StoreResult<Option<MirrorRequestRecord>> {
    let mut stmt = conn.prepare_cached(sql).map_err(sql_err)?;
    let mut rows = stmt.query(args).map_err(sql_err)?;
    match rows.next().map_err(sql_err)? {
        Some(r) => record(r).map(Some),
        None => Ok(None),
    }
}

pub(crate) fn get(
    conn: &Connection,
    repo: i64,
    id: u64,
) -> StoreResult<Option<MirrorRequestRecord>> {
    one(
        conn,
        &format!("SELECT {COLUMNS} FROM mirror_requests WHERE repo = ?1 AND id = ?2"),
        params![repo, int(id)?],
    )
}

pub(crate) fn find(
    conn: &Connection,
    repo: i64,
    kind: MirrorRequestKind,
    client_request_id: &str,
) -> StoreResult<Option<MirrorRequestRecord>> {
    one(
        conn,
        &format!(
            "SELECT {COLUMNS} FROM mirror_requests
             WHERE repo = ?1 AND kind = ?2 AND client_request_id = ?3"
        ),
        params![repo, kind.as_str(), client_request_id],
    )
}

pub(crate) fn version(conn: &Connection, repo: i64, id: u64) -> StoreResult<Option<u64>> {
    let id = int(id)?;
    let v: Option<i64> = conn
        .prepare_cached("SELECT version FROM mirror_requests WHERE repo = ?1 AND id = ?2")
        .and_then(|mut s| s.query_row(params![repo, id], |r| r.get(0)).optional())
        .map_err(sql_err)?;
    v.map(uint).transpose()
}

pub(crate) fn key(
    conn: &Connection,
    repo: i64,
    kind: MirrorRequestKind,
    client_request_id: &str,
) -> StoreResult<Option<u64>> {
    let v: Option<i64> = conn
        .prepare_cached(
            "SELECT id FROM mirror_requests
             WHERE repo = ?1 AND kind = ?2 AND client_request_id = ?3",
        )
        .and_then(|mut s| {
            s.query_row(params![repo, kind.as_str(), client_request_id], |r| {
                r.get(0)
            })
            .optional()
        })
        .map_err(sql_err)?;
    v.map(uint).transpose()
}

pub(crate) fn slot(
    conn: &Connection,
    repo: i64,
    line: &str,
    slot: LineSlot,
) -> StoreResult<Option<u64>> {
    let v: Option<i64> = conn
        .prepare_cached(
            "SELECT request FROM mirror_line_slots WHERE repo = ?1 AND line = ?2 AND slot = ?3",
        )
        .and_then(|mut s| {
            s.query_row(params![repo, line, slot.as_str()], |r| r.get(0))
                .optional()
        })
        .map_err(sql_err)?;
    v.map(uint).transpose()
}

pub(crate) fn last_id(conn: &Connection, repo: i64) -> StoreResult<u64> {
    let v: Option<i64> = conn
        .prepare_cached("SELECT MAX(id) FROM mirror_requests WHERE repo = ?1")
        .and_then(|mut s| s.query_row(params![repo], |r| r.get(0)))
        .map_err(sql_err)?;
    v.map_or(Ok(0), uint)
}

pub(crate) fn list(
    conn: &Connection,
    repo: i64,
    q: &MirrorRequestQuery,
) -> StoreResult<Vec<MirrorRequestRecord>> {
    let mut sql = format!("SELECT {COLUMNS} FROM mirror_requests WHERE repo = ?");
    let mut args: Vec<Value> = vec![Value::Integer(repo)];
    if let Some(k) = q.kind {
        sql.push_str(" AND kind = ?");
        args.push(Value::Text(k.as_str().to_owned()));
    }
    if let Some(l) = &q.line {
        sql.push_str(" AND line = ?");
        args.push(Value::Text(l.clone()));
    }
    if !q.states.is_empty() {
        sql.push_str(" AND state IN (");
        for (i, s) in q.states.iter().enumerate() {
            sql.push_str(if i == 0 { "?" } else { ", ?" });
            args.push(Value::Text(s.as_str().to_owned()));
        }
        sql.push(')');
    }
    if let Some(a) = q.after {
        sql.push_str(if q.oldest_first {
            " AND id > ?"
        } else {
            " AND id < ?"
        });
        args.push(Value::Integer(int(a)?));
    }
    sql.push_str(if q.oldest_first {
        " ORDER BY id ASC LIMIT ?"
    } else {
        " ORDER BY id DESC LIMIT ?"
    });
    args.push(Value::Integer(page_limit(q.limit) as i64));
    let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
    let mut rows = stmt.query(params_from_iter(args)).map_err(sql_err)?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(sql_err)? {
        out.push(record(r)?);
    }
    Ok(out)
}

pub(crate) fn put(conn: &Connection, repo: i64, r: &MirrorRequestRecord) -> StoreResult<()> {
    let id = int(r.id)?;
    let epoch = r.claimed_by_epoch.map(int).transpose()?;
    let pr = r.result.pr_number.map(int).transpose()?;
    let version = int(r.version)?;
    conn.prepare_cached(
        "INSERT INTO mirror_requests (repo, id, line, kind, client_request_id, base, excludes,
             upstream, state, batch_state, claimed_by_epoch, claimed_at, deadline, pr_number,
             pr_url, branch, lands, error_code, requested_by, created_at, updated_at, version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
             ?18, ?19, ?20, ?21, ?22)
         ON CONFLICT (repo, id) DO UPDATE SET state = excluded.state,
             batch_state = excluded.batch_state, claimed_by_epoch = excluded.claimed_by_epoch,
             claimed_at = excluded.claimed_at, deadline = excluded.deadline,
             pr_number = excluded.pr_number, pr_url = excluded.pr_url, branch = excluded.branch,
             lands = excluded.lands, error_code = excluded.error_code,
             updated_at = excluded.updated_at, version = excluded.version",
    )
    .and_then(|mut s| {
        s.execute(params![
            repo,
            id,
            r.line,
            r.kind.as_str(),
            r.client_request_id,
            r.base.map(|b| b.0.0.to_vec()),
            ids_blob(&r.excludes),
            r.upstream,
            r.state.as_str(),
            r.batch.map(BatchState::as_str),
            epoch,
            r.claimed_at_ms,
            r.deadline_ms,
            pr,
            r.result.pr_url,
            r.result.branch,
            ids_blob(&r.result.lands),
            r.result.error_code,
            r.requested_by,
            r.created_at_ms,
            r.updated_at_ms,
            version,
        ])
    })
    .map_err(sql_err)?;
    Ok(())
}

pub(crate) fn set_slot(
    conn: &Connection,
    repo: i64,
    line: &str,
    slot: LineSlot,
    request: Option<u64>,
) -> StoreResult<()> {
    let request = request.map(int).transpose()?;
    match request {
        Some(id) => conn
            .prepare_cached(
                "INSERT INTO mirror_line_slots (repo, line, slot, request) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (repo, line, slot) DO UPDATE SET request = excluded.request",
            )
            .and_then(|mut s| s.execute(params![repo, line, slot.as_str(), id])),
        None => conn
            .prepare_cached(
                "DELETE FROM mirror_line_slots WHERE repo = ?1 AND line = ?2 AND slot = ?3",
            )
            .and_then(|mut s| s.execute(params![repo, line, slot.as_str()])),
    }
    .map_err(sql_err)?;
    Ok(())
}
