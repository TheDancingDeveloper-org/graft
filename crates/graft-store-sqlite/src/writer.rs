//! The writer thread: the one connection that writes.
//!
//! Jobs arrive on a bounded queue. The thread takes every job waiting (up to
//! [`MAX_BATCH`]) and runs them in one SQLite transaction, each job inside its
//! own savepoint: a job that fails or whose precondition does not hold rolls
//! back to its savepoint and leaves the others alone. One `COMMIT` (one WAL
//! fsync under `synchronous=FULL`) then makes the whole group durable, and
//! only after it does any caller get its answer. So every logical write is
//! still atomic on its own, and every acknowledged write is durable.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;

use graft_store::{MetaTxn, Precondition, RepoId, StoreError, StoreResult, TxnOutcome, Write};
use rusqlite::{Connection, DropBehavior, OptionalExtension, TransactionBehavior, params};

use crate::codec::{int, ref_kind_code, sql_err, state_code, uint};
use crate::oneshot;
use crate::reader::{check_repo, read_ref};

/// The most jobs grouped under one commit.
pub(crate) const MAX_BATCH: usize = 256;

/// A write the store accepts.
#[derive(Debug)]
pub(crate) enum WriteOp {
    CreateRepo(String),
    Commit(RepoId, MetaTxn),
}

/// What a write returns.
#[derive(Debug)]
pub(crate) enum Written {
    Repo(RepoId),
    Txn(TxnOutcome),
}

impl Written {
    /// Whether the job's writes are kept.
    fn keeps(&self) -> bool {
        matches!(
            self,
            Written::Repo(_) | Written::Txn(TxnOutcome::Committed(_))
        )
    }
}

/// A queued write and where its answer goes.
#[derive(Debug)]
pub(crate) struct WriteJob {
    pub(crate) op: WriteOp,
    pub(crate) reply: oneshot::Sender<StoreResult<Written>>,
}

/// The writer thread's loop. It ends when every sender is gone.
pub(crate) fn run(mut conn: Connection, jobs: Receiver<WriteJob>) {
    while let Ok(first) = jobs.recv() {
        let mut batch = vec![first];
        while batch.len() < MAX_BATCH {
            match jobs.try_recv() {
                Ok(job) => batch.push(job),
                Err(_) => break,
            }
        }
        commit_batch(&mut conn, batch);
    }
}

/// Runs a group of jobs under one commit and answers each.
fn commit_batch(conn: &mut Connection, batch: Vec<WriteJob>) {
    let (ops, replies): (Vec<_>, Vec<_>) = batch.into_iter().map(|j| (j.op, j.reply)).unzip();
    match apply_batch(conn, ops) {
        Ok(results) => {
            for (reply, result) in replies.into_iter().zip(results) {
                reply.send(result);
            }
        }
        Err(err) => {
            for reply in replies {
                reply.send(Err(err.clone()));
            }
        }
    }
}

/// Applies the jobs and commits. The outer error means nothing was kept.
fn apply_batch(conn: &mut Connection, ops: Vec<WriteOp>) -> StoreResult<Vec<StoreResult<Written>>> {
    let mut tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_err)?;
    let mut results = Vec::with_capacity(ops.len());
    for op in ops {
        let mut sp = tx.savepoint().map_err(sql_err)?;
        let result = apply(&sp, op);
        let keep = result.as_ref().is_ok_and(Written::keeps);
        sp.set_drop_behavior(if keep {
            DropBehavior::Commit
        } else {
            DropBehavior::Rollback
        });
        sp.finish().map_err(sql_err)?;
        results.push(result);
    }
    tx.commit().map_err(sql_err)?;
    Ok(results)
}

fn apply(conn: &Connection, op: WriteOp) -> StoreResult<Written> {
    match op {
        WriteOp::CreateRepo(name) => create_repo(conn, &name).map(Written::Repo),
        WriteOp::Commit(repo, txn) => commit(conn, repo, txn).map(Written::Txn),
    }
}

fn create_repo(conn: &Connection, name: &str) -> StoreResult<RepoId> {
    let taken = conn
        .prepare_cached("SELECT 1 FROM repos WHERE name = ?1")
        .and_then(|mut s| s.exists([name]))
        .map_err(sql_err)?;
    if taken {
        return Err(StoreError::Exists);
    }
    conn.prepare_cached("INSERT INTO repos (name) VALUES (?1)")
        .and_then(|mut s| s.execute([name]))
        .map_err(sql_err)?;
    let id = conn.last_insert_rowid();
    u32::try_from(id)
        .map(RepoId)
        .map_err(|_| StoreError::Backend(format!("repository id {id} is out of range")))
}

/// Whether a precondition holds in the state the transaction sees.
fn holds(conn: &Connection, repo: i64, pre: &Precondition) -> StoreResult<bool> {
    match pre {
        Precondition::RefIs { name, expected } => {
            let current = read_ref(conn, repo, name)?.map(|r| (r.target, r.version));
            Ok(current == *expected)
        }
        Precondition::WorkspaceHeadIs { name, head } => {
            let row: Option<Option<Vec<u8>>> = conn
                .prepare_cached("SELECT head FROM workspaces WHERE repo = ?1 AND name = ?2")
                .and_then(|mut s| s.query_row(params![repo, name], |r| r.get(0)).optional())
                .map_err(sql_err)?;
            Ok(match row {
                None => false,
                Some(stored) => stored.as_deref() == head.as_ref().map(|h| &h.0.0[..]),
            })
        }
        Precondition::ChangesetAbsent(id) => {
            let present = conn
                .prepare_cached("SELECT 1 FROM changesets WHERE repo = ?1 AND id = ?2")
                .and_then(|mut s| s.exists(params![repo, &id.0.0[..]]))
                .map_err(sql_err)?;
            Ok(!present)
        }
    }
}

/// One logical write. Runs inside its own savepoint, so an error or a failed
/// precondition leaves nothing behind.
fn commit(conn: &Connection, repo_id: RepoId, txn: MetaTxn) -> StoreResult<TxnOutcome> {
    let repo = i64::from(repo_id.0);
    check_repo(conn, repo_id)?;
    for (index, pre) in txn.preconditions.iter().enumerate() {
        if !holds(conn, repo, pre)? {
            return Ok(TxnOutcome::PreconditionFailed(index));
        }
    }

    let has_journal = txn
        .writes
        .iter()
        .any(|w| matches!(w, Write::Journal { .. }));
    let last: i64 = conn
        .prepare_cached("SELECT coalesce(max(lsn), 0) FROM journal WHERE repo = ?1")
        .and_then(|mut s| s.query_row([repo], |r| r.get(0)))
        .map_err(sql_err)?;
    let first_lsn = uint(last)? + 1;
    let mut next_lsn = first_lsn;
    let mut lsns = Vec::new();
    // Ref versions as this transaction's writes leave them.
    let mut versions: HashMap<String, u64> = HashMap::new();

    for write in txn.writes {
        match write {
            Write::PutChangeset {
                id,
                content,
                mut record,
            } => {
                if has_journal {
                    record.lsn = first_lsn;
                }
                let lsn = int(record.lsn)?;
                conn.prepare_cached(
                    "INSERT INTO changesets
                         (repo, id, content, lsn, received_at, state, holder, superseded_by)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT (repo, id) DO UPDATE SET
                         content = excluded.content, lsn = excluded.lsn,
                         received_at = excluded.received_at, state = excluded.state,
                         holder = excluded.holder, superseded_by = excluded.superseded_by",
                )
                .and_then(|mut s| {
                    s.execute(params![
                        repo,
                        &id.0.0[..],
                        content,
                        lsn,
                        record.received_at_ms,
                        state_code(record.state),
                        record.holder,
                        record.superseded_by.map(|c| c.0.0.to_vec()),
                    ])
                })
                .map_err(sql_err)?;
            }
            Write::SetRecord { id, mut record } => {
                if has_journal {
                    record.lsn = first_lsn;
                }
                let lsn = int(record.lsn)?;
                let changed = conn
                    .prepare_cached(
                        "UPDATE changesets SET lsn = ?3, received_at = ?4, state = ?5,
                             holder = ?6, superseded_by = ?7
                         WHERE repo = ?1 AND id = ?2",
                    )
                    .and_then(|mut s| {
                        s.execute(params![
                            repo,
                            &id.0.0[..],
                            lsn,
                            record.received_at_ms,
                            state_code(record.state),
                            record.holder,
                            record.superseded_by.map(|c| c.0.0.to_vec()),
                        ])
                    })
                    .map_err(sql_err)?;
                if changed == 0 {
                    return Err(StoreError::Invalid(format!("no changeset {id} to update")));
                }
            }
            Write::SetRef(record) => {
                let guarded = txn
                    .preconditions
                    .iter()
                    .any(|p| matches!(p, Precondition::RefIs { name, .. } if *name == record.name));
                if !guarded {
                    return Err(StoreError::Invalid(format!(
                        "ref {} is set without a RefIs precondition",
                        record.name
                    )));
                }
                let current = match versions.get(&record.name) {
                    Some(v) => *v,
                    None => read_ref(conn, repo, &record.name)?.map_or(0, |r| r.version),
                };
                if record.version != current + 1 {
                    return Err(StoreError::Invalid(format!(
                        "ref {} version {} does not follow {current}",
                        record.name, record.version
                    )));
                }
                let version = int(record.version)?;
                conn.prepare_cached(
                    "INSERT INTO refs (repo, name, target, version, kind)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (repo, name) DO UPDATE SET
                         target = excluded.target, version = excluded.version,
                         kind = excluded.kind",
                )
                .and_then(|mut s| {
                    s.execute(params![
                        repo,
                        record.name,
                        &record.target.0.0[..],
                        version,
                        ref_kind_code(record.kind),
                    ])
                })
                .map_err(sql_err)?;
                versions.insert(record.name, record.version);
            }
            Write::PutWorkspace(ws) => {
                conn.prepare_cached(
                    "INSERT INTO workspaces
                         (repo, name, owner, session, line, base, head, lease_until)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT (repo, name) DO UPDATE SET
                         owner = excluded.owner, session = excluded.session,
                         line = excluded.line, base = excluded.base,
                         head = excluded.head, lease_until = excluded.lease_until",
                )
                .and_then(|mut s| {
                    s.execute(params![
                        repo,
                        ws.name,
                        ws.owner,
                        ws.session,
                        ws.line,
                        &ws.base.0.0[..],
                        ws.head.map(|h| h.0.0.to_vec()),
                        ws.lease_until_ms,
                    ])
                })
                .map_err(sql_err)?;
            }
            Write::DeleteWorkspace(name) => {
                conn.prepare_cached("DELETE FROM workspaces WHERE repo = ?1 AND name = ?2")
                    .and_then(|mut s| s.execute(params![repo, name]))
                    .map_err(sql_err)?;
            }
            Write::PutDerived {
                kind,
                version,
                key,
                value,
            } => {
                conn.prepare_cached(
                    "INSERT INTO derived (repo, kind, ver, key, value)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (repo, kind, ver, key) DO UPDATE SET value = excluded.value",
                )
                .and_then(|mut s| s.execute(params![repo, kind, version, key, value]))
                .map_err(sql_err)?;
            }
            Write::PutLinePos {
                line,
                changeset,
                pos,
            } => {
                let pos = int(pos)?;
                conn.prepare_cached(
                    "INSERT INTO line_pos (repo, line, changeset, pos) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (repo, line, changeset) DO UPDATE SET pos = excluded.pos",
                )
                .and_then(|mut s| s.execute(params![repo, line, &changeset.0.0[..], pos]))
                .map_err(sql_err)?;
            }
            Write::PutLinePath {
                line,
                path,
                pos,
                changeset,
            } => {
                let pos = int(pos)?;
                conn.prepare_cached(
                    "INSERT OR IGNORE INTO line_paths (repo, line, path, pos, changeset)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .and_then(|mut s| {
                    s.execute(params![repo, line, path.as_str(), pos, &changeset.0.0[..]])
                })
                .map_err(sql_err)?;
            }
            Write::AddHolder { object, holder } => {
                conn.prepare_cached(
                    "INSERT OR IGNORE INTO holders (repo, object, holder) VALUES (?1, ?2, ?3)",
                )
                .and_then(|mut s| s.execute(params![repo, &object.0[..], holder]))
                .map_err(sql_err)?;
            }
            Write::RemoveHolder { object, holder } => {
                conn.prepare_cached(
                    "DELETE FROM holders WHERE repo = ?1 AND object = ?2 AND holder = ?3",
                )
                .and_then(|mut s| s.execute(params![repo, &object.0[..], holder]))
                .map_err(sql_err)?;
            }
            Write::PutLand {
                draft,
                line,
                landed,
            } => {
                conn.prepare_cached(
                    "INSERT OR IGNORE INTO lands (repo, draft, line, landed)
                     VALUES (?1, ?2, ?3, ?4)",
                )
                .and_then(|mut s| s.execute(params![repo, &draft.0.0[..], line, &landed.0.0[..]]))
                .map_err(sql_err)?;
            }
            Write::Journal { kind, body } => {
                let lsn = int(next_lsn)?;
                conn.prepare_cached(
                    "INSERT INTO journal (repo, lsn, kind, body) VALUES (?1, ?2, ?3, ?4)",
                )
                .and_then(|mut s| s.execute(params![repo, lsn, kind, body]))
                .map_err(sql_err)?;
                lsns.push(next_lsn);
                next_lsn += 1;
            }
        }
    }
    Ok(TxnOutcome::Committed(lsns))
}
