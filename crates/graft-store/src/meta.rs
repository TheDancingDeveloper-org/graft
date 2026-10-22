//! The `MetaStore` port: single-key reads, single-record CAS, and
//! transactions scoped to one logical write plus its journal entry.

use std::future::Future;

use graft_model::{ChangesetId, ObjectId, RepoPath};

use crate::{RepoId, StoreResult};

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

/// A condition a transaction checks before writing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Precondition {
    /// The ref holds this target (or is absent, for `None`).
    RefIs {
        /// Ref name.
        name: String,
        /// Expected target.
        target: Option<ChangesetId>,
    },
    /// The workspace head is this (or none).
    WorkspaceHeadIs {
        /// Workspace name.
        name: String,
        /// Expected head.
        head: Option<ChangesetId>,
    },
    /// The changeset does not exist yet (idempotent insert reports Exists).
    ChangesetAbsent(ChangesetId),
}

/// One write inside a transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    /// Insert content and its record.
    PutChangeset {
        /// Id.
        id: ChangesetId,
        /// Canonical encoding.
        content: Vec<u8>,
        /// Record.
        record: ChangesetRecord,
    },
    /// Update a changeset's record.
    SetRecord {
        /// Id.
        id: ChangesetId,
        /// New record.
        record: ChangesetRecord,
    },
    /// Set a ref (after a `RefIs` precondition).
    SetRef(RefRecord),
    /// Upsert a workspace.
    PutWorkspace(WorkspaceRecord),
    /// Delete a workspace.
    DeleteWorkspace(String),
    /// Store a derived value.
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
    /// Append a line-position row.
    PutLinePos {
        /// Line.
        line: String,
        /// Changeset.
        changeset: ChangesetId,
        /// Position.
        pos: u64,
    },
    /// Append a line path index row.
    PutLinePath {
        /// Line.
        line: String,
        /// Path or directory prefix.
        path: RepoPath,
        /// Position.
        pos: u64,
    },
    /// Add an object holder row.
    AddHolder {
        /// Object.
        object: ObjectId,
        /// Holder (a changeset id or an upload lease).
        holder: String,
    },
    /// Remove an object holder row.
    RemoveHolder {
        /// Object.
        object: ObjectId,
        /// Holder.
        holder: String,
    },
    /// Record an idempotent land.
    PutLand {
        /// Draft head.
        draft: ChangesetId,
        /// Line.
        line: String,
        /// Result.
        landed: ChangesetId,
    },
    /// Append a journal entry; the store assigns the LSN.
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
    /// Creates a repository namespace, returning its id.
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
}
