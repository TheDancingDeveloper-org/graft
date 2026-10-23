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
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use graft_model::{ChangesetId, ObjectId};
use graft_store::{
    MetaTxn, Precondition, RecordState, RepoId, StoreError, StoreResult, TxnOutcome, Write,
};
use rusqlite::{Connection, DropBehavior, OptionalExtension, TransactionBehavior, params};

use crate::WriterQueue;
use crate::codec::{int, ref_kind_code, sql_err, state_code, uint};
use crate::oneshot;
use crate::reader::{changeset, check_repo, doom_mark, read_protection, read_ref, workspace};

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

/// The writer thread's loop. It ends when every sender is gone. `queue`
/// counts the batches it has finished taking off the queue, before it runs
/// them.
pub(crate) fn run(mut conn: Connection, jobs: Receiver<WriteJob>, queue: Arc<WriterQueue>) {
    while let Ok(first) = jobs.recv() {
        let mut batch = vec![first];
        while batch.len() < MAX_BATCH {
            match jobs.try_recv() {
                Ok(job) => batch.push(job),
                Err(_) => break,
            }
        }
        queue.taken(batch.len());
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
    // A new repository has protection records from its first write.
    conn.prepare_cached("INSERT INTO repos (name, gc_protection) VALUES (?1, 1)")
        .and_then(|mut s| s.execute([name]))
        .map_err(sql_err)?;
    let id = conn.last_insert_rowid();
    u32::try_from(id)
        .map(RepoId)
        .map_err(|_| StoreError::Backend(format!("repository id {id} is out of range")))
}

/// Whether a precondition holds in the state the transaction sees.
fn holds(conn: &Connection, repo_id: RepoId, pre: &Precondition) -> StoreResult<bool> {
    let repo = i64::from(repo_id.0);
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
        Precondition::WorkspaceIs { name, expected } => {
            let current = workspace(conn, repo_id, name)?;
            Ok(current == *expected)
        }
        Precondition::RecordStateIs { id, state } => {
            let row: Option<i64> = conn
                .prepare_cached("SELECT state FROM changesets WHERE repo = ?1 AND id = ?2")
                .and_then(|mut s| {
                    s.query_row(params![repo, &id.0.0[..]], |r| r.get(0))
                        .optional()
                })
                .map_err(sql_err)?;
            Ok(row == Some(state_code(*state)))
        }
        Precondition::ChangesetAbsent(id) => {
            let present = conn
                .prepare_cached("SELECT 1 FROM changesets WHERE repo = ?1 AND id = ?2")
                .and_then(|mut s| s.exists(params![repo, &id.0.0[..]]))
                .map_err(sql_err)?;
            Ok(!present)
        }
        Precondition::RecordLive(id) => {
            let state = record_state(conn, repo, *id)?;
            Ok(state.is_some_and(|s| s != state_code(RecordState::Collected)))
        }
        Precondition::RecordIs {
            id,
            record,
            protection,
        } => Ok(
            changeset(conn, repo_id, *id)?.is_some_and(|(_, r)| r == *record)
                && read_protection(conn, repo, *id)? == Some(*protection),
        ),
        Precondition::ObjectDoomed(object) => {
            Ok(doom_mark(conn, repo, *object)?.is_some_and(|m| !m.deleting))
        }
        Precondition::ObjectUnheld(object) => Ok(!is_held(conn, repo, *object)?),
        Precondition::DerivedIs {
            kind,
            version,
            key,
            value,
        } => Ok(crate::reader::derived(conn, repo_id, kind, *version, key)? == *value),
    }
}

/// A record's state code, or `None` when it is not stored.
fn record_state(conn: &Connection, repo: i64, id: ChangesetId) -> StoreResult<Option<i64>> {
    conn.prepare_cached("SELECT state FROM changesets WHERE repo = ?1 AND id = ?2")
        .and_then(|mut s| {
            s.query_row(params![repo, &id.0.0[..]], |r| r.get(0))
                .optional()
        })
        .map_err(sql_err)
}

fn is_held(conn: &Connection, repo: i64, object: ObjectId) -> StoreResult<bool> {
    conn.prepare_cached("SELECT 1 FROM holders WHERE repo = ?1 AND object = ?2 LIMIT 1")
        .and_then(|mut s| s.exists(params![repo, &object.0[..]]))
        .map_err(sql_err)
}

/// Whether a record in state `code` keeps protection: landed and collected
/// records have none.
fn protectable(code: i64) -> bool {
    code != state_code(RecordState::Landed) && code != state_code(RecordState::Collected)
}

fn invalid(what: String) -> StoreError {
    StoreError::Invalid(what)
}

/// Adds a holder row (with an expiry, for a timed one) and revives the
/// object: a held object is not doomed, unless it is being deleted.
fn add_row(
    conn: &Connection,
    repo: i64,
    object: ObjectId,
    holder: &str,
    expires: Option<i64>,
) -> StoreResult<()> {
    conn.prepare_cached(
        "INSERT OR IGNORE INTO holders (repo, object, holder, expires) VALUES (?1, ?2, ?3, ?4)",
    )
    .and_then(|mut s| s.execute(params![repo, &object.0[..], holder, expires]))
    .map_err(sql_err)?;
    conn.prepare_cached("DELETE FROM doomed WHERE repo = ?1 AND object = ?2 AND deleting = 0")
        .and_then(|mut s| s.execute(params![repo, &object.0[..]]))
        .map_err(sql_err)?;
    Ok(())
}

/// Applies one protection or object-mark write.
fn protect(conn: &Connection, repo: i64, write: Write) -> StoreResult<()> {
    match write {
        Write::SetExpiry { id, expires_at_ms } => match record_state(conn, repo, id)? {
            None => return Err(invalid(format!("no changeset {id} to expire"))),
            Some(code) if !protectable(code) && expires_at_ms.is_some() => {
                return Err(invalid(format!("changeset {id} has landed: no expiry")));
            }
            Some(_) => {
                conn.prepare_cached(
                    "UPDATE changesets SET expires_at = ?3 WHERE repo = ?1 AND id = ?2",
                )
                .and_then(|mut s| s.execute(params![repo, &id.0.0[..], expires_at_ms]))
                .map_err(sql_err)?;
            }
        },
        Write::Pin(id) | Write::Unpin(id) => {
            let pin = matches!(write, Write::Pin(_));
            let code = record_state(conn, repo, id)?
                .filter(|c| *c != state_code(RecordState::Collected))
                .ok_or_else(|| invalid(format!("no live changeset {id} to pin")))?;
            if code == state_code(RecordState::Landed) {
                return Ok(());
            }
            let pins = read_protection(conn, repo, id)?.map_or(0, |p| p.pins);
            let next = if pin {
                pins.checked_add(1)
                    .ok_or_else(|| invalid(format!("changeset {id} has {pins} pins")))?
            } else {
                pins.saturating_sub(1)
            };
            set_pins(conn, repo, id, next)?;
        }
        Write::SetPins { id, pins } => match record_state(conn, repo, id)? {
            None => return Err(invalid(format!("no changeset {id} to pin"))),
            Some(code) if !protectable(code) && pins > 0 => {
                return Err(invalid(format!("changeset {id} has landed: no pins")));
            }
            Some(_) => set_pins(conn, repo, id, pins)?,
        },
        Write::AddHolderUntil {
            object,
            holder,
            expires_at_ms,
        } => add_row(conn, repo, object, &holder, Some(expires_at_ms))?,
        Write::Doom { object, since_ms } => {
            if is_held(conn, repo, object)? {
                return Err(invalid(format!("object {} is held", object.to_hex())));
            }
            if doom_mark(conn, repo, object)?.is_some_and(|m| m.deleting) {
                return Err(invalid(format!(
                    "object {} is being deleted",
                    object.to_hex()
                )));
            }
            conn.prepare_cached(
                "INSERT OR IGNORE INTO doomed (repo, object, since, deleting)
                 VALUES (?1, ?2, ?3, 0)",
            )
            .and_then(|mut s| s.execute(params![repo, &object.0[..], since_ms]))
            .map_err(sql_err)?;
        }
        Write::MarkDeleting(object) => {
            let changed = conn
                .prepare_cached("UPDATE doomed SET deleting = 1 WHERE repo = ?1 AND object = ?2")
                .and_then(|mut s| s.execute(params![repo, &object.0[..]]))
                .map_err(sql_err)?;
            if changed == 0 {
                return Err(invalid(format!("object {} is not doomed", object.to_hex())));
            }
        }
        Write::Undoom(object) => {
            conn.prepare_cached("DELETE FROM doomed WHERE repo = ?1 AND object = ?2")
                .and_then(|mut s| s.execute(params![repo, &object.0[..]]))
                .map_err(sql_err)?;
        }
        Write::SetGcProtection(on) => {
            conn.prepare_cached("UPDATE repos SET gc_protection = ?2 WHERE id = ?1")
                .and_then(|mut s| s.execute(params![repo, on]))
                .map_err(sql_err)?;
        }
        other => {
            return Err(StoreError::Backend(format!(
                "not a protection write: {other:?}"
            )));
        }
    }
    Ok(())
}

fn set_pins(conn: &Connection, repo: i64, id: ChangesetId, pins: u32) -> StoreResult<()> {
    conn.prepare_cached("UPDATE changesets SET pins = ?3 WHERE repo = ?1 AND id = ?2")
        .and_then(|mut s| s.execute(params![repo, &id.0.0[..], pins]))
        .map_err(sql_err)?;
    Ok(())
}

/// One logical write. Runs inside its own savepoint, so an error or a failed
/// precondition leaves nothing behind.
fn commit(conn: &Connection, repo_id: RepoId, txn: MetaTxn) -> StoreResult<TxnOutcome> {
    let repo = i64::from(repo_id.0);
    check_repo(conn, repo_id)?;
    for (index, pre) in txn.preconditions.iter().enumerate() {
        if !holds(conn, repo_id, pre)? {
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
                         (repo, id, content, lsn, received_at, state, holder, superseded_by,
                          pins, expires_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, NULL)
                     ON CONFLICT (repo, id) DO UPDATE SET
                         content = excluded.content, lsn = excluded.lsn,
                         received_at = excluded.received_at, state = excluded.state,
                         holder = excluded.holder, superseded_by = excluded.superseded_by,
                         pins = 0, expires_at = NULL",
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
                             holder = ?6, superseded_by = ?7,
                             pins = CASE WHEN ?5 IN (?8, ?9) THEN 0 ELSE pins END,
                             expires_at = CASE WHEN ?5 IN (?8, ?9) THEN NULL ELSE expires_at END
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
                            state_code(RecordState::Landed),
                            state_code(RecordState::Collected),
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
            Write::PutLineFork { line, parent, pos } => {
                let pos = int(pos)?;
                conn.prepare_cached(
                    "INSERT INTO line_forks (repo, line, parent, pos) VALUES (?1, ?2, ?3, ?4)",
                )
                .and_then(|mut s| s.execute(params![repo, line, parent, pos]))
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
            Write::AddHolder { object, holder } => add_row(conn, repo, object, &holder, None)?,
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
            write @ (Write::AddHolderUntil { .. }
            | Write::SetExpiry { .. }
            | Write::Pin(_)
            | Write::Unpin(_)
            | Write::SetPins { .. }
            | Write::Doom { .. }
            | Write::MarkDeleting(_)
            | Write::Undoom(_)
            | Write::SetGcProtection(_)) => protect(conn, repo, write)?,
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
