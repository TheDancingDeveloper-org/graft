//! The `repo_mirrors` table (D41): one row per `(repo, upstream)`.
//!
//! Lines and the export author map are stored as text, one entry per
//! line, fields separated by a tab. The boundary rejects control characters
//! in every field, and encoding refuses them again rather than store a row
//! that would not read back.

use graft_store::{
    LeaseRecord, MirrorDirection, MirrorExportRecord, MirrorLineRecord, MirrorListing, MirrorQuery,
    MirrorRecord, MirrorStatusRecord, RepoId, StoreError, StoreResult, page_limit,
};
use rusqlite::{Connection, OptionalExtension as _, Row, params};

use crate::codec::{changeset_id, int, sql_err, uint};

const COLUMNS: &str = "m.upstream, m.lines, m.interval_s, m.enabled, m.credential, m.kek_id,
     m.export_default_author, m.export_authors, m.version, m.created_by, m.updated_by,
     m.updated_at, m.status_set, m.status_poll_at, m.status_success_at, m.status_sha,
     m.status_changeset, m.status_lag_changesets, m.status_lag_s, m.status_error_code,
     m.status_error_message, m.status_backoff_until, m.status_lease_epoch";

fn field(s: &str) -> StoreResult<&str> {
    if s.contains(['\t', '\n']) {
        return Err(StoreError::Invalid(
            "a mirror field holds a tab or newline".into(),
        ));
    }
    Ok(s)
}

fn encode_lines(lines: &[MirrorLineRecord]) -> StoreResult<String> {
    let mut out = String::new();
    for l in lines {
        out.push_str(field(&l.branch)?);
        out.push('\t');
        out.push_str(field(&l.line)?);
        out.push('\t');
        out.push_str(l.direction.as_str());
        out.push('\n');
    }
    Ok(out)
}

fn decode_lines(text: &str) -> StoreResult<Vec<MirrorLineRecord>> {
    text.lines()
        .map(|row| {
            let mut parts = row.split('\t');
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some(branch), Some(line), Some(dir), None) => Ok(MirrorLineRecord {
                    branch: branch.to_owned(),
                    line: line.to_owned(),
                    direction: MirrorDirection::parse(dir)
                        .ok_or_else(|| StoreError::Corrupt(format!("mirror direction {dir:?}")))?,
                }),
                _ => Err(StoreError::Corrupt("mirror lines".into())),
            }
        })
        .collect()
}

fn encode_authors(authors: &[(String, String)]) -> StoreResult<String> {
    let mut out = String::new();
    for (principal, identity) in authors {
        out.push_str(field(principal)?);
        out.push('\t');
        out.push_str(field(identity)?);
        out.push('\n');
    }
    Ok(out)
}

fn decode_authors(text: &str) -> StoreResult<Vec<(String, String)>> {
    text.lines()
        .map(|row| {
            row.split_once('\t')
                .map(|(p, i)| (p.to_owned(), i.to_owned()))
                .ok_or_else(|| StoreError::Corrupt("mirror export authors".into()))
        })
        .collect()
}

fn opt_uint(v: Option<i64>) -> StoreResult<Option<u64>> {
    v.map(uint).transpose()
}

/// The record whose columns (`COLUMNS`) start at index `o`.
fn record(r: &Row<'_>, o: usize) -> rusqlite::Result<StoreResult<MirrorRecord>> {
    let lines: String = r.get(o + 1)?;
    let interval: i64 = r.get(o + 2)?;
    let authors: String = r.get(o + 7)?;
    let version: i64 = r.get(o + 8)?;
    let status_set: bool = r.get(o + 12)?;
    let changeset: Option<Vec<u8>> = r.get(o + 16)?;
    let lag_cs: i64 = r.get(o + 17)?;
    let lag_s: i64 = r.get(o + 18)?;
    let epoch: i64 = r.get(o + 22)?;
    let poll_at: Option<i64> = r.get(o + 13)?;
    let success_at: Option<i64> = r.get(o + 14)?;
    let sha: Option<String> = r.get(o + 15)?;
    let code: Option<String> = r.get(o + 19)?;
    let message: Option<String> = r.get(o + 20)?;
    let backoff: Option<i64> = r.get(o + 21)?;
    let upstream: String = r.get(o)?;
    let enabled: bool = r.get(o + 3)?;
    let credential: Option<Vec<u8>> = r.get(o + 4)?;
    let kek_id: Option<String> = r.get(o + 5)?;
    let default_author: Option<String> = r.get(o + 6)?;
    let created_by: String = r.get(o + 9)?;
    let updated_by: String = r.get(o + 10)?;
    let updated_at_ms: i64 = r.get(o + 11)?;
    Ok((|| {
        let status = if status_set {
            Some(MirrorStatusRecord {
                last_poll_at_ms: poll_at,
                last_success_at_ms: success_at,
                last_imported_sha: sha,
                last_changeset: changeset.as_deref().map(changeset_id).transpose()?,
                lag_changesets: uint(lag_cs)?,
                lag_seconds: uint(lag_s)?,
                error_code: code,
                error_message: message,
                backoff_until_ms: backoff,
                lease_epoch: uint(epoch)?,
            })
        } else {
            None
        };
        Ok(MirrorRecord {
            upstream,
            lines: decode_lines(&lines)?,
            interval_s: u32::try_from(interval)
                .map_err(|_| StoreError::Corrupt("mirror interval".into()))?,
            enabled,
            credential,
            kek_id,
            export: MirrorExportRecord {
                default_author,
                authors: decode_authors(&authors)?,
            },
            version: uint(version)?,
            status,
            created_by,
            updated_by,
            updated_at_ms,
        })
    })())
}

/// The stored version of `(repo, upstream)`, if the row exists.
pub(crate) fn version(conn: &Connection, repo: i64, upstream: &str) -> StoreResult<Option<u64>> {
    let v: Option<i64> = conn
        .prepare_cached("SELECT version FROM repo_mirrors WHERE repo = ?1 AND upstream = ?2")
        .and_then(|mut s| {
            s.query_row(params![repo, upstream], |r| r.get(0))
                .optional()
        })
        .map_err(sql_err)?;
    opt_uint(v)
}

pub(crate) fn get(
    conn: &Connection,
    repo: RepoId,
    upstream: &str,
) -> StoreResult<Option<MirrorRecord>> {
    let sql = format!("SELECT {COLUMNS} FROM repo_mirrors m WHERE m.repo = ?1 AND m.upstream = ?2");
    conn.prepare_cached(&sql)
        .and_then(|mut s| {
            s.query_row(params![i64::from(repo.0), upstream], |r| record(r, 0))
                .optional()
        })
        .map_err(sql_err)?
        .transpose()
}

pub(crate) fn list(conn: &Connection, q: &MirrorQuery) -> StoreResult<Vec<MirrorListing>> {
    let limit = page_limit(q.limit) as i64;
    let (after_repo, after_up) = match &q.after {
        Some((r, u)) => (i64::from(r.0), Some(u.as_str())),
        None => (0, None),
    };
    // Keyset pagination on the primary key (repo, upstream).
    let sql = format!(
        "SELECT r.id, r.name, {COLUMNS} FROM repo_mirrors m JOIN repos r ON r.id = m.repo
         WHERE (?1 IS NULL OR m.repo = ?1)
           AND (?2 IS NULL OR m.enabled = ?2)
           AND (m.repo > ?3 OR (m.repo = ?3 AND (?4 IS NULL OR m.upstream > ?4)))
         ORDER BY m.repo, m.upstream LIMIT ?5"
    );
    let mut stmt = conn.prepare_cached(&sql).map_err(sql_err)?;
    let rows = stmt
        .query_map(
            params![
                q.repo.map(|r| i64::from(r.0)),
                q.enabled,
                after_repo,
                after_up,
                limit
            ],
            |r| {
                let id: i64 = r.get(0)?;
                let name: String = r.get(1)?;
                Ok((id, name, record(r, 2)?))
            },
        )
        .map_err(sql_err)?;
    let mut out = Vec::new();
    for row in rows {
        let (id, repo_name, rec) = row.map_err(sql_err)?;
        out.push(MirrorListing {
            repo: RepoId(
                u32::try_from(id).map_err(|_| StoreError::Corrupt("repository id".into()))?,
            ),
            repo_name,
            record: rec?,
        });
    }
    Ok(out)
}

pub(crate) fn put(conn: &Connection, repo: i64, m: &MirrorRecord) -> StoreResult<()> {
    let lines = encode_lines(&m.lines)?;
    let authors = encode_authors(&m.export.authors)?;
    let version = int(m.version)?;
    conn.prepare_cached(
        "INSERT INTO repo_mirrors
             (repo, upstream, lines, interval_s, enabled, credential, kek_id,
              export_default_author, export_authors, version, created_by, updated_by,
              updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT (repo, upstream) DO UPDATE SET
             lines = excluded.lines, interval_s = excluded.interval_s,
             enabled = excluded.enabled, credential = excluded.credential,
             kek_id = excluded.kek_id,
             export_default_author = excluded.export_default_author,
             export_authors = excluded.export_authors, version = excluded.version,
             updated_by = excluded.updated_by, updated_at = excluded.updated_at",
    )
    .and_then(|mut s| {
        s.execute(params![
            repo,
            m.upstream,
            lines,
            i64::from(m.interval_s),
            m.enabled,
            m.credential,
            m.kek_id,
            m.export.default_author,
            authors,
            version,
            m.created_by,
            m.updated_by,
            m.updated_at_ms,
        ])
    })
    .map_err(sql_err)?;
    Ok(())
}

pub(crate) fn delete(conn: &Connection, repo: i64, upstream: &str) -> StoreResult<()> {
    conn.prepare_cached("DELETE FROM repo_mirrors WHERE repo = ?1 AND upstream = ?2")
        .and_then(|mut s| s.execute(params![repo, upstream]))
        .map_err(sql_err)?;
    Ok(())
}

pub(crate) fn set_status(
    conn: &Connection,
    repo: i64,
    upstream: &str,
    st: &MirrorStatusRecord,
) -> StoreResult<()> {
    let (lag_cs, lag_s, epoch) = (
        int(st.lag_changesets)?,
        int(st.lag_seconds)?,
        int(st.lease_epoch)?,
    );
    conn.prepare_cached(
        "UPDATE repo_mirrors SET status_set = 1, status_poll_at = ?3,
             status_success_at = ?4, status_sha = ?5, status_changeset = ?6,
             status_lag_changesets = ?7, status_lag_s = ?8, status_error_code = ?9,
             status_error_message = ?10, status_backoff_until = ?11,
             status_lease_epoch = ?12
         WHERE repo = ?1 AND upstream = ?2",
    )
    .and_then(|mut s| {
        s.execute(params![
            repo,
            upstream,
            st.last_poll_at_ms,
            st.last_success_at_ms,
            st.last_imported_sha,
            st.last_changeset.map(|c| c.0.0.to_vec()),
            lag_cs,
            lag_s,
            st.error_code,
            st.error_message,
            st.backoff_until_ms,
            epoch,
        ])
    })
    .map_err(sql_err)?;
    Ok(())
}

/// The repository's lease row (D41 §3.6), if it ever had one.
pub(crate) fn lease(conn: &Connection, repo: i64) -> StoreResult<Option<LeaseRecord>> {
    let row: Option<(Option<String>, i64, i64)> = conn
        .prepare_cached("SELECT holder, epoch, expires_at FROM mirror_leases WHERE repo = ?1")
        .and_then(|mut s| {
            s.query_row(params![repo], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()
        })
        .map_err(sql_err)?;
    row.map(|(holder, epoch, expires_at_ms)| {
        Ok(LeaseRecord {
            holder,
            epoch: uint(epoch)?,
            expires_at_ms,
        })
    })
    .transpose()
}

/// The stored epoch, 0 when the repository never had a lease.
pub(crate) fn lease_epoch(conn: &Connection, repo: i64) -> StoreResult<u64> {
    Ok(lease(conn, repo)?.map_or(0, |l| l.epoch))
}

pub(crate) fn put_lease(conn: &Connection, repo: i64, l: &LeaseRecord) -> StoreResult<()> {
    let epoch = int(l.epoch)?;
    conn.prepare_cached(
        "INSERT INTO mirror_leases (repo, holder, epoch, expires_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (repo) DO UPDATE SET holder = excluded.holder,
             epoch = excluded.epoch, expires_at = excluded.expires_at",
    )
    .and_then(|mut s| s.execute(params![repo, l.holder, epoch, l.expires_at_ms]))
    .map_err(sql_err)?;
    Ok(())
}
