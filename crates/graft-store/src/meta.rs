//! The `MetaStore` port: single-key reads, single-record CAS, and
//! transactions scoped to one logical write plus its journal entry.

use std::future::Future;

use graft_model::{ChangesetId, ObjectId, RepoPath};

use crate::{RepoId, StoreError, StoreResult};

/// A journal position.
pub type Lsn = u64;

/// Kind of ref.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    /// Protected line.
    Line,
    /// Immutable tag.
    Tag,
    /// Tool-owned.
    Tool,
}

/// A stored ref.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefRecord {
    /// Name.
    pub name: String,
    /// Target.
    pub target: ChangesetId,
    /// Version.
    pub version: u64,
    /// Kind.
    pub kind: RefKind,
}

/// Lifecycle state stored with a changeset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordState {
    /// Held.
    Draft,
    /// On a line.
    Landed,
    /// Replaced.
    Superseded,
    /// Unheld.
    Abandoned,
    /// Content removed; tombstone only.
    Collected,
}

/// The mutable record kept beside a changeset's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangesetRecord {
    /// Journal position of creation.
    pub lsn: Lsn,
    /// Server receive time, ms.
    pub received_at_ms: i64,
    /// State.
    pub state: RecordState,
    /// Holder: `ws:<name>` or `land:<attempt>`.
    pub holder: Option<String>,
    /// Successor when superseded.
    pub superseded_by: Option<ChangesetId>,
}

/// A stored workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceRecord {
    /// Name.
    pub name: String,
    /// Owner subject.
    pub owner: String,
    /// Vogt session.
    pub session: Option<String>,
    /// Tracked line.
    pub line: String,
    /// Base.
    pub base: ChangesetId,
    /// Head.
    pub head: Option<ChangesetId>,
    /// Lease expiry, ms.
    pub lease_until_ms: i64,
    /// Vogt work item, from the opening token's claims.
    pub work_item: Option<String>,
}

/// The GC protection kept beside a changeset's record (design §8.6). It is
/// read and written apart from [`ChangesetRecord`]: [`Write::Pin`],
/// [`Write::Unpin`], [`Write::SetPins`] and [`Write::SetExpiry`] change it,
/// and a record written as landed or collected always has the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Protection {
    /// On a non-landed record, the number of non-collected changesets that
    /// name it as a parent. A record with pins is never collected. Always 0
    /// on a landed record.
    pub pins: u32,
    /// When the record may be collected, ms: set when it stops being a
    /// draft, or when a land-attempt candidate is written. `None` for drafts
    /// held by a workspace and for landed records.
    pub expires_at_ms: Option<i64>,
}

/// The mark on an object that no row holds (design §8.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DoomMark {
    /// When it was doomed, ms.
    pub since_ms: i64,
    /// A sweep is deleting its frame. A write that needs the object must
    /// wait for the delete to finish (or report it missing); it cannot claim
    /// it.
    pub deleting: bool,
}

/// A holder row with an expiry: an upload lease or a write's claim.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimedRow {
    /// When the row lapses, ms.
    pub expires_at_ms: i64,
    /// Object.
    pub object: ObjectId,
    /// Holder.
    pub holder: String,
}

/// A condition a transaction checks before writing. The first failing
/// precondition aborts the transaction, and nothing is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Precondition {
    /// The ref is at this `(target, version)`, or absent for `None`. This is
    /// the compare in ref CAS: a stale target or version fails.
    RefIs {
        /// Ref name.
        name: String,
        /// Expected `(target, version)`.
        expected: Option<(ChangesetId, u64)>,
    },
    /// The workspace exists and its head is this. An absent workspace never
    /// satisfies it, whatever `head` is.
    WorkspaceHeadIs {
        /// Workspace name.
        name: String,
        /// Expected head (`None` means no head).
        head: Option<ChangesetId>,
    },
    /// The changeset is not stored yet. This makes an insert idempotent: a
    /// retry fails this precondition and writes nothing.
    ChangesetAbsent(ChangesetId),
    /// The workspace is exactly `expected`, or absent for `None`. This is the
    /// compare in a whole-record workspace CAS (open, renew, close).
    WorkspaceIs {
        /// Workspace name.
        name: String,
        /// Expected record.
        expected: Option<WorkspaceRecord>,
    },
    /// The changeset is stored and its record is in `state`. This guards a
    /// record transition against a concurrent one.
    RecordStateIs {
        /// Changeset.
        id: ChangesetId,
        /// Expected state.
        state: RecordState,
    },
    /// The changeset is stored and not collected. A new child checks this
    /// on each parent it pins, so it cannot race the parent's collection.
    RecordLive(ChangesetId),
    /// The changeset is stored with exactly this record and protection. The
    /// compare in a collection: any transition, adoption or new pin since
    /// the read fails it.
    RecordIs {
        /// Changeset.
        id: ChangesetId,
        /// Expected record.
        record: ChangesetRecord,
        /// Expected protection.
        protection: Protection,
    },
    /// The object is doomed and not being deleted. A claim and a delete
    /// mark both check it, so at most one of them wins.
    ObjectDoomed(ObjectId),
    /// No row holds the object.
    ObjectUnheld(ObjectId),
    /// The derived value at `(kind, version, key)` is `value`, or absent
    /// for `None`. The compare in a derived-value CAS (the GC sweep lease).
    DerivedIs {
        /// Kind name.
        kind: String,
        /// Kind version.
        version: u32,
        /// Key bytes.
        key: Vec<u8>,
        /// Expected value.
        value: Option<Vec<u8>>,
    },
}

/// One write inside a transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    /// Insert content and its record. Stores set `record.lsn` to the LSN of
    /// the transaction's first `Journal` write, when there is one. The new
    /// record has no pins and no expiry. A stored changeset is replaced
    /// outright: the sweep writes a collected tombstone with no content
    /// this way, and a write of a collected changeset revives it (both
    /// guarded by a precondition on the record).
    PutChangeset {
        /// Id.
        id: ChangesetId,
        /// Canonical encoding.
        content: Vec<u8>,
        /// Record.
        record: ChangesetRecord,
    },
    /// Update a changeset's record (`record.lsn` is handled as for
    /// `PutChangeset`). Its protection is kept, except that a record set to
    /// landed or collected gets no pins and no expiry (design §8.6).
    SetRecord {
        /// Id.
        id: ChangesetId,
        /// New record.
        record: ChangesetRecord,
    },
    /// Set a ref. It must follow a `RefIs` precondition on the same name,
    /// and its `version` must be the current version plus one (1 for a new
    /// ref). Anything else is `StoreError::Invalid`.
    SetRef(RefRecord),
    /// Upsert a workspace.
    PutWorkspace(WorkspaceRecord),
    /// Delete a workspace, and its draft path rows. Deleting an absent one
    /// is a no-op.
    DeleteWorkspace(String),
    /// Replaces a workspace's draft path rows on `line` with `paths`: the
    /// union of the paths its stack changes (`docs/notes/draft-overlap.md`
    /// §3). Empty `paths` removes them. Cost follows the rows removed and
    /// added.
    SetDraftPaths {
        /// Workspace.
        workspace: String,
        /// The line it tracks.
        line: String,
        /// Changed paths.
        paths: Vec<RepoPath>,
    },
    /// Adds draft path rows for a workspace on `line` (a path already there
    /// counts once).
    AddDraftPaths {
        /// Workspace.
        workspace: String,
        /// The line it tracks.
        line: String,
        /// Changed paths.
        paths: Vec<RepoPath>,
    },
    /// Store a derived value (overwrites an existing one for the same key).
    PutDerived {
        /// Kind name.
        kind: String,
        /// Kind version.
        version: u32,
        /// Key bytes.
        key: Vec<u8>,
        /// Value bytes.
        value: Vec<u8>,
    },
    /// Sets a changeset's position on a line (overwrites).
    PutLinePos {
        /// Line.
        line: String,
        /// Changeset.
        changeset: ChangesetId,
        /// Position.
        pos: u64,
    },
    /// Records that a new line forks from `parent` at position `pos`: the
    /// line's own rows start at `pos + 1`, and reads below that fall through
    /// to `parent` (`docs/notes/line-fork-point.md`). Written once, when the
    /// line is created.
    PutLineFork {
        /// The new line.
        line: String,
        /// The line it forks from.
        parent: String,
        /// The fork point's position, on both lines.
        pos: u64,
    },
    /// Adds a line path index row for a path or directory prefix. The
    /// engine writes one row per prefix of each changed path.
    PutLinePath {
        /// Line.
        line: String,
        /// Path or directory prefix.
        path: RepoPath,
        /// Position.
        pos: u64,
        /// The changeset at that position that touched `path`.
        changeset: ChangesetId,
    },
    /// Adds an object holder row. A holder is a set member, so adding the
    /// same `(object, holder)` twice counts once. A held object is not
    /// doomed: adding a row clears the object's doomed mark, unless it is
    /// being deleted (design §8.6, revival).
    AddHolder {
        /// Object.
        object: ObjectId,
        /// Holder (a changeset id or an upload lease).
        holder: String,
    },
    /// Removes an object holder row. Removing an absent row is a no-op.
    RemoveHolder {
        /// Object.
        object: ObjectId,
        /// Holder.
        holder: String,
    },
    /// Adds a holder row that lapses at `expires_at_ms`: an upload lease or
    /// a claim. Otherwise as [`Write::AddHolder`]; the first add of a row
    /// decides its expiry.
    AddHolderUntil {
        /// Object.
        object: ObjectId,
        /// Holder.
        holder: String,
        /// Expiry, ms.
        expires_at_ms: i64,
    },
    /// Sets a changeset's expiry. The changeset must be stored, and a
    /// landed or collected one takes only `None`; anything else is
    /// `StoreError::Invalid`.
    SetExpiry {
        /// Changeset.
        id: ChangesetId,
        /// Expiry, ms.
        expires_at_ms: Option<i64>,
    },
    /// Adds one pin to a parent, unless it has landed, when nothing changes:
    /// the store decides by the state inside the transaction. A parent that
    /// is absent or collected is `StoreError::Invalid`.
    Pin(ChangesetId),
    /// Takes one pin off a parent, unless it has landed, when nothing
    /// changes. A parent with no pins keeps 0: its pins were never written
    /// (a repository the backfill has not reached). A parent that is absent
    /// or collected is `StoreError::Invalid`.
    Unpin(ChangesetId),
    /// Sets a changeset's pins outright (the backfill and `fsck`). The
    /// changeset must be stored, and a landed one takes only 0.
    SetPins {
        /// Changeset.
        id: ChangesetId,
        /// Pins.
        pins: u32,
    },
    /// Marks an object doomed since `since_ms`. An object already doomed
    /// keeps its first time. An object with a row, or being deleted, is
    /// `StoreError::Invalid`.
    Doom {
        /// Object.
        object: ObjectId,
        /// Time, ms.
        since_ms: i64,
    },
    /// Marks a doomed object as being deleted. An object that is not doomed
    /// is `StoreError::Invalid`.
    MarkDeleting(ObjectId),
    /// Removes an object's doomed mark, `deleting` or not (after its frame
    /// is deleted). Removing an absent mark is a no-op.
    Undoom(ObjectId),
    /// Sets the repository's `gc_protection` marker: its records carry
    /// complete protection, so a sweep may run on it.
    SetGcProtection(bool),
    /// Records a land. The first record for `(draft, line)` is kept, so a
    /// retried land returns the same result.
    PutLand {
        /// Draft head.
        draft: ChangesetId,
        /// Line.
        line: String,
        /// Result.
        landed: ChangesetId,
    },
    /// Append a journal entry. The store assigns the next LSN of the
    /// repository (LSNs start at 1 and have no gaps).
    Journal {
        /// Entry kind.
        kind: String,
        /// Entry body.
        body: Vec<u8>,
    },
}

/// A transaction: preconditions, then writes, applied atomically.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetaTxn {
    /// All must hold.
    pub preconditions: Vec<Precondition>,
    /// Applied in order if they do.
    pub writes: Vec<Write>,
}

/// The result of a transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxnOutcome {
    /// Applied; the LSNs of its journal entries.
    Committed(Vec<Lsn>),
    /// A precondition failed (index into `preconditions`); nothing written.
    PreconditionFailed(usize),
}

/// Metadata storage.
pub trait MetaStore: Send + Sync {
    /// Creates a repository namespace, returning its id. A name that is
    /// already taken is `StoreError::Exists`.
    fn create_repo(&self, name: &str) -> impl Future<Output = StoreResult<RepoId>> + Send;
    /// Looks up a repository by name.
    fn repo(&self, name: &str) -> impl Future<Output = StoreResult<Option<RepoId>>> + Send;
    /// Applies a transaction atomically.
    fn commit(
        &self,
        repo: RepoId,
        txn: MetaTxn,
    ) -> impl Future<Output = StoreResult<TxnOutcome>> + Send;
    /// Applies a run of transactions in order, each atomically on its own
    /// (its own unit, with its own journal entries), stopping at the first
    /// that is not `Committed`. Returns one outcome per attempted
    /// transaction: all `Committed`, or ending with the one that failed;
    /// later ones were not attempted. A transaction sees the writes of the
    /// ones before it in the run, so a run may chain (a changeset, then its
    /// child). Nothing is acknowledged before every committed one is
    /// durable. On `Err`, a prefix of the run may have been committed.
    ///
    /// The default commits them one by one; a store may group the run
    /// under one durable commit (one sync instead of one per transaction).
    fn commit_run(
        &self,
        repo: RepoId,
        txns: Vec<MetaTxn>,
    ) -> impl Future<Output = StoreResult<Vec<TxnOutcome>>> + Send {
        async move {
            let mut out = Vec::with_capacity(txns.len());
            for txn in txns {
                let outcome = self.commit(repo, txn).await?;
                let committed = matches!(outcome, TxnOutcome::Committed(_));
                out.push(outcome);
                if !committed {
                    break;
                }
            }
            Ok(out)
        }
    }
    /// Reads a changeset's content and record.
    fn changeset(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> impl Future<Output = StoreResult<Option<(Vec<u8>, ChangesetRecord)>>> + Send;
    /// Reads a ref.
    fn get_ref(
        &self,
        repo: RepoId,
        name: &str,
    ) -> impl Future<Output = StoreResult<Option<RefRecord>>> + Send;
    /// Lists refs by prefix.
    fn list_refs(
        &self,
        repo: RepoId,
        prefix: &str,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<RefRecord>>> + Send;
    /// Reads a workspace.
    fn workspace(
        &self,
        repo: RepoId,
        name: &str,
    ) -> impl Future<Output = StoreResult<Option<WorkspaceRecord>>> + Send;
    /// Lists workspaces whose lease ends before `before_ms` (for GC).
    fn expired_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send;
    /// Lists `owner`'s workspaces in name order, names after `after` (all
    /// from the start for `""`), up to `limit`.
    fn list_workspaces(
        &self,
        repo: RepoId,
        owner: &str,
        after: &str,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send;
    /// Counts the changesets whose record is in `state`. The cost follows
    /// that population (drafts, superseded, abandoned), so it is not for
    /// `Landed`.
    fn count_records(
        &self,
        repo: RepoId,
        state: RecordState,
    ) -> impl Future<Output = StoreResult<u64>> + Send;
    /// Counts workspaces: all of them, or with `Some(before_ms)` those whose
    /// lease ends before it.
    fn count_workspaces(
        &self,
        repo: RepoId,
        before_ms: Option<i64>,
    ) -> impl Future<Output = StoreResult<u64>> + Send;
    /// Reads a derived value.
    fn derived(
        &self,
        repo: RepoId,
        kind: &str,
        version: u32,
        key: &[u8],
    ) -> impl Future<Output = StoreResult<Option<Vec<u8>>>> + Send;
    /// Position of a changeset on a line, falling through a line's fork
    /// point to its parent (one read per hop).
    fn line_pos(
        &self,
        repo: RepoId,
        line: &str,
        id: ChangesetId,
    ) -> impl Future<Output = StoreResult<Option<u64>>> + Send;
    /// The lines that hold `id` in their own position rows (not through a
    /// fork), with its position, in line order, up to `limit`. Stores
    /// answer it with a lookup by changeset. The default, for wrappers,
    /// reads one page of refs and `line_pos` per line, so it may name a line
    /// that holds `id` through a fork.
    fn line_holders(
        &self,
        repo: RepoId,
        id: ChangesetId,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(String, u64)>>> + Send {
        async move {
            let mut out = Vec::new();
            for r in self.list_refs(repo, "", crate::MAX_PAGE).await? {
                if r.kind != RefKind::Line {
                    continue;
                }
                if let Some(pos) = self.line_pos(repo, &r.name, id).await? {
                    out.push((r.name, pos));
                    if out.len() >= crate::page_limit(limit) {
                        break;
                    }
                }
            }
            Ok(out)
        }
    }
    /// Line path index scan, newest first, positions below `before`,
    /// continuing on the parent line below a fork point.
    fn scan_line_path(
        &self,
        repo: RepoId,
        line: &str,
        path: &RepoPath,
        before: u64,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(u64, ChangesetId)>>> + Send;
    /// Holder count of an object.
    fn holders(
        &self,
        repo: RepoId,
        object: ObjectId,
    ) -> impl Future<Output = StoreResult<u32>> + Send;
    /// A previous land result.
    fn land(
        &self,
        repo: RepoId,
        draft: ChangesetId,
        line: &str,
    ) -> impl Future<Output = StoreResult<Option<ChangesetId>>> + Send;
    /// Journal entries from `from` (inclusive), up to `limit`.
    fn journal(
        &self,
        repo: RepoId,
        from: Lsn,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(Lsn, String, Vec<u8>)>>> + Send;
    /// Changesets whose record is in `state`, in id order after `after`
    /// (from the start for `None`), up to `limit`. The cost follows that
    /// population, never the whole changeset table (design §8.6). A store
    /// without this index answers `StoreError::Backend`.
    fn list_records(
        &self,
        _repo: RepoId,
        _state: RecordState,
        _after: Option<ChangesetId>,
        _limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(ChangesetId, ChangesetRecord)>>> + Send {
        async { Err(unsupported("list_records")) }
    }
    /// Every workspace of the repository in name order, names after `after`
    /// (from the start for `""`), up to `limit`. A store without this index
    /// answers `StoreError::Backend`.
    fn list_all_workspaces(
        &self,
        _repo: RepoId,
        _after: &str,
        _limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send {
        async { Err(unsupported("list_all_workspaces")) }
    }
    /// Holder names of `object` in name order after `after` (from the start
    /// for `None`), up to `limit`. Its length equals [`MetaStore::holders`]
    /// when the limit is not reached. A store without this index answers
    /// `StoreError::Backend`.
    fn holder_rows(
        &self,
        _repo: RepoId,
        _object: ObjectId,
        _after: Option<String>,
        _limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<String>>> + Send {
        async { Err(unsupported("holder_rows")) }
    }
    /// A changeset's protection, or `None` when it is not stored. A store
    /// without protection records answers `StoreError::Backend`.
    fn protection(
        &self,
        _repo: RepoId,
        _id: ChangesetId,
    ) -> impl Future<Output = StoreResult<Option<Protection>>> + Send {
        async { Err(unsupported("protection")) }
    }
    /// The records a collection may take now: no pins and `expires_at_ms`
    /// at or before `now_ms`, in `(expires_at_ms, id)` order after `after`,
    /// up to `limit`. One index range over exactly those records.
    fn expired_records(
        &self,
        _repo: RepoId,
        _now_ms: i64,
        _after: Option<(i64, ChangesetId)>,
        _limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(i64, ChangesetId)>>> + Send {
        async { Err(unsupported("expired_records")) }
    }
    /// The objects `holder` holds rows for, in id order after `after`, up
    /// to `limit` (the holder index).
    fn held_objects(
        &self,
        _repo: RepoId,
        _holder: &str,
        _after: Option<ObjectId>,
        _limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<ObjectId>>> + Send {
        async { Err(unsupported("held_objects")) }
    }
    /// Holder rows with an expiry at or before `now_ms`, in
    /// `(expires_at_ms, object, holder)` order after `after`, up to `limit`.
    fn expired_rows(
        &self,
        _repo: RepoId,
        _now_ms: i64,
        _after: Option<TimedRow>,
        _limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<TimedRow>>> + Send {
        async { Err(unsupported("expired_rows")) }
    }
    /// The doomed marks of those of `ids` that have one, in the order of
    /// `ids`. One key read per id: the caller bounds `ids`.
    fn doom_marks(
        &self,
        _repo: RepoId,
        _ids: &[ObjectId],
    ) -> impl Future<Output = StoreResult<Vec<(ObjectId, DoomMark)>>> + Send {
        async { Err(unsupported("doom_marks")) }
    }
    /// Objects doomed at or before `until_ms`, in `(since_ms, object)` order
    /// after `after`, up to `limit`.
    fn old_doomed(
        &self,
        _repo: RepoId,
        _until_ms: i64,
        _after: Option<(i64, ObjectId)>,
        _limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(ObjectId, DoomMark)>>> + Send {
        async { Err(unsupported("old_doomed")) }
    }
    /// Workspaces whose lease ends before `before_ms`, in
    /// `(lease_until_ms, name)` order after `after`, up to `limit`.
    fn lapsed_workspaces(
        &self,
        _repo: RepoId,
        _before_ms: i64,
        _after: Option<(i64, String)>,
        _limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send {
        async { Err(unsupported("lapsed_workspaces")) }
    }
    /// Draft path rows `(path, workspace)` on `line` whose path is `prefix`
    /// or lies beneath it, in `(path, workspace)` order after `after`, up to
    /// `limit`. One index range: the cost follows the rows returned.
    fn scan_draft_paths(
        &self,
        _repo: RepoId,
        _line: &str,
        _prefix: &RepoPath,
        _after: Option<(RepoPath, String)>,
        _limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(RepoPath, String)>>> + Send {
        async { Err(unsupported("scan_draft_paths")) }
    }
    /// A workspace's draft path rows in path order after `after`, up to
    /// `limit` (the reverse index; for `fsck` and tests).
    fn workspace_draft_paths(
        &self,
        _repo: RepoId,
        _workspace: &str,
        _after: Option<RepoPath>,
        _limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<RepoPath>>> + Send {
        async { Err(unsupported("workspace_draft_paths")) }
    }
    /// Whether the repository has the `gc_protection` marker. A repository
    /// created by a store with protection records has it from the start.
    fn gc_protection(&self, _repo: RepoId) -> impl Future<Output = StoreResult<bool>> + Send {
        async { Err(unsupported("gc_protection")) }
    }
}

/// The error a store answers for an optional index it does not have yet.
fn unsupported(what: &str) -> StoreError {
    StoreError::Backend(format!("{what} is not implemented by this store"))
}

impl<T: MetaStore> MetaStore for std::sync::Arc<T> {
    fn create_repo(&self, name: &str) -> impl Future<Output = StoreResult<RepoId>> + Send {
        (**self).create_repo(name)
    }
    fn repo(&self, name: &str) -> impl Future<Output = StoreResult<Option<RepoId>>> + Send {
        (**self).repo(name)
    }
    fn commit(
        &self,
        repo: RepoId,
        txn: MetaTxn,
    ) -> impl Future<Output = StoreResult<TxnOutcome>> + Send {
        (**self).commit(repo, txn)
    }
    fn commit_run(
        &self,
        repo: RepoId,
        txns: Vec<MetaTxn>,
    ) -> impl Future<Output = StoreResult<Vec<TxnOutcome>>> + Send {
        (**self).commit_run(repo, txns)
    }
    fn changeset(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> impl Future<Output = StoreResult<Option<(Vec<u8>, ChangesetRecord)>>> + Send {
        (**self).changeset(repo, id)
    }
    fn get_ref(
        &self,
        repo: RepoId,
        name: &str,
    ) -> impl Future<Output = StoreResult<Option<RefRecord>>> + Send {
        (**self).get_ref(repo, name)
    }
    fn list_refs(
        &self,
        repo: RepoId,
        prefix: &str,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<RefRecord>>> + Send {
        (**self).list_refs(repo, prefix, limit)
    }
    fn workspace(
        &self,
        repo: RepoId,
        name: &str,
    ) -> impl Future<Output = StoreResult<Option<WorkspaceRecord>>> + Send {
        (**self).workspace(repo, name)
    }
    fn expired_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send {
        (**self).expired_workspaces(repo, before_ms, limit)
    }
    fn list_workspaces(
        &self,
        repo: RepoId,
        owner: &str,
        after: &str,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send {
        (**self).list_workspaces(repo, owner, after, limit)
    }
    fn count_records(
        &self,
        repo: RepoId,
        state: RecordState,
    ) -> impl Future<Output = StoreResult<u64>> + Send {
        (**self).count_records(repo, state)
    }
    fn count_workspaces(
        &self,
        repo: RepoId,
        before_ms: Option<i64>,
    ) -> impl Future<Output = StoreResult<u64>> + Send {
        (**self).count_workspaces(repo, before_ms)
    }
    fn derived(
        &self,
        repo: RepoId,
        kind: &str,
        version: u32,
        key: &[u8],
    ) -> impl Future<Output = StoreResult<Option<Vec<u8>>>> + Send {
        (**self).derived(repo, kind, version, key)
    }
    fn line_pos(
        &self,
        repo: RepoId,
        line: &str,
        id: ChangesetId,
    ) -> impl Future<Output = StoreResult<Option<u64>>> + Send {
        (**self).line_pos(repo, line, id)
    }
    fn line_holders(
        &self,
        repo: RepoId,
        id: ChangesetId,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(String, u64)>>> + Send {
        (**self).line_holders(repo, id, limit)
    }
    fn scan_line_path(
        &self,
        repo: RepoId,
        line: &str,
        path: &RepoPath,
        before: u64,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(u64, ChangesetId)>>> + Send {
        (**self).scan_line_path(repo, line, path, before, limit)
    }
    fn holders(
        &self,
        repo: RepoId,
        object: ObjectId,
    ) -> impl Future<Output = StoreResult<u32>> + Send {
        (**self).holders(repo, object)
    }
    fn land(
        &self,
        repo: RepoId,
        draft: ChangesetId,
        line: &str,
    ) -> impl Future<Output = StoreResult<Option<ChangesetId>>> + Send {
        (**self).land(repo, draft, line)
    }
    fn journal(
        &self,
        repo: RepoId,
        from: Lsn,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(Lsn, String, Vec<u8>)>>> + Send {
        (**self).journal(repo, from, limit)
    }
    fn list_records(
        &self,
        repo: RepoId,
        state: RecordState,
        after: Option<ChangesetId>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(ChangesetId, ChangesetRecord)>>> + Send {
        (**self).list_records(repo, state, after, limit)
    }
    fn holder_rows(
        &self,
        repo: RepoId,
        object: ObjectId,
        after: Option<String>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<String>>> + Send {
        (**self).holder_rows(repo, object, after, limit)
    }
    fn list_all_workspaces(
        &self,
        repo: RepoId,
        after: &str,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send {
        (**self).list_all_workspaces(repo, after, limit)
    }
    fn protection(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> impl Future<Output = StoreResult<Option<Protection>>> + Send {
        (**self).protection(repo, id)
    }
    fn expired_records(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<(i64, ChangesetId)>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(i64, ChangesetId)>>> + Send {
        (**self).expired_records(repo, now_ms, after, limit)
    }
    fn held_objects(
        &self,
        repo: RepoId,
        holder: &str,
        after: Option<ObjectId>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<ObjectId>>> + Send {
        (**self).held_objects(repo, holder, after, limit)
    }
    fn expired_rows(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<TimedRow>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<TimedRow>>> + Send {
        (**self).expired_rows(repo, now_ms, after, limit)
    }
    fn doom_marks(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
    ) -> impl Future<Output = StoreResult<Vec<(ObjectId, DoomMark)>>> + Send {
        (**self).doom_marks(repo, ids)
    }
    fn old_doomed(
        &self,
        repo: RepoId,
        until_ms: i64,
        after: Option<(i64, ObjectId)>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(ObjectId, DoomMark)>>> + Send {
        (**self).old_doomed(repo, until_ms, after, limit)
    }
    fn lapsed_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        after: Option<(i64, String)>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send {
        (**self).lapsed_workspaces(repo, before_ms, after, limit)
    }
    fn scan_draft_paths(
        &self,
        repo: RepoId,
        line: &str,
        prefix: &RepoPath,
        after: Option<(RepoPath, String)>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(RepoPath, String)>>> + Send {
        (**self).scan_draft_paths(repo, line, prefix, after, limit)
    }
    fn workspace_draft_paths(
        &self,
        repo: RepoId,
        workspace: &str,
        after: Option<RepoPath>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<RepoPath>>> + Send {
        (**self).workspace_draft_paths(repo, workspace, after, limit)
    }
    fn gc_protection(&self, repo: RepoId) -> impl Future<Output = StoreResult<bool>> + Send {
        (**self).gc_protection(repo)
    }
}
