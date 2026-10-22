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
}

/// One write inside a transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    /// Insert content and its record. Stores set `record.lsn` to the LSN of
    /// the transaction's first `Journal` write, when there is one.
    PutChangeset {
        /// Id.
        id: ChangesetId,
        /// Canonical encoding.
        content: Vec<u8>,
        /// Record.
        record: ChangesetRecord,
    },
    /// Update a changeset's record (`record.lsn` is handled as for
    /// `PutChangeset`).
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
    /// Delete a workspace. Deleting an absent one is a no-op.
    DeleteWorkspace(String),
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
    /// same `(object, holder)` twice counts once.
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
    /// Position of a changeset on a line.
    fn line_pos(
        &self,
        repo: RepoId,
        line: &str,
        id: ChangesetId,
    ) -> impl Future<Output = StoreResult<Option<u64>>> + Send;
    /// Line path index scan, newest first, positions below `before`.
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
}
