//! Derived data and history queries (`docs/design.md` §4.9, §7.5, §7.7).
//!
//! Work items G1.4 (framework and eager kinds) and G1.10 (queries). Eager
//! kinds produce [`graft_store::Write`] rows that the engine commits in the
//! same transaction as the write they derive from.

use graft_model::{Changeset, ChangesetId, RepoPath};
use graft_store::{MetaStore, ObjectStore, RepoId, StoreResult, Write};

pub mod kinds;

/// Which parents a derived kind reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParentScope {
    /// `parents[0]` only.
    First,
    /// Every parent.
    All,
}

/// When a derived kind is computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Before the write is acknowledged.
    Eager,
    /// On first use, walking back to the nearest computed value.
    Lazy,
}

/// A derived kind's identity and shape.
pub trait DerivedKind {
    /// Stable name, part of the storage key.
    const NAME: &'static str;
    /// Version; bumping it recomputes lazily.
    const VERSION: u32;
    /// Parents read.
    const PARENTS: ParentScope;
    /// Eager or lazy.
    const MODE: Mode;
}

/// A history query (the engine maps `graft_proto::types::Log` onto it).
#[derive(Clone, Debug)]
pub struct LogQuery {
    /// Newest changeset.
    pub from: ChangesetId,
    /// Exclusive stop.
    pub until: Option<ChangesetId>,
    /// Path or directory filters.
    pub paths: Vec<RepoPath>,
    /// Maximum results.
    pub limit: u32,
    /// Resume after this line position.
    pub after_pos: Option<u64>,
}

/// One blame run: lines `[start, end)` came from `origin`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameRun {
    /// First line.
    pub start: u32,
    /// One past the last line.
    pub end: u32,
    /// Origin changeset.
    pub origin: ChangesetId,
}

/// The index over a repository's stores.
#[derive(Debug)]
pub struct Index<M, O> {
    meta: M,
    objects: O,
}

impl<M: MetaStore, O: ObjectStore> Index<M, O> {
    /// Wraps the stores.
    pub fn new(meta: M, objects: O) -> Self {
        Self { meta, objects }
    }

    /// The metadata store.
    pub fn meta(&self) -> &M {
        &self.meta
    }

    /// The object store.
    pub fn objects(&self) -> &O {
        &self.objects
    }

    /// Eager rows for a newly created changeset (generation, skip list).
    pub async fn on_create(&self, _repo: RepoId, _cs: &Changeset) -> StoreResult<Vec<Write>> {
        todo!("G1.4")
    }

    /// Eager rows for changesets landing on `line` (positions, path index).
    pub async fn on_land(
        &self,
        _repo: RepoId,
        _line: &str,
        _landed: &[Changeset],
    ) -> StoreResult<Vec<Write>> {
        todo!("G1.4")
    }

    /// First-parent history, newest first.
    pub async fn log(&self, _repo: RepoId, _q: LogQuery) -> StoreResult<Vec<ChangesetId>> {
        todo!("G1.10")
    }

    /// Whether `a` is an ancestor of `b` (all parents).
    pub async fn is_ancestor(
        &self,
        _repo: RepoId,
        _a: ChangesetId,
        _b: ChangesetId,
    ) -> StoreResult<bool> {
        todo!("G1.10")
    }

    /// A merge base of `a` and `b`.
    pub async fn merge_base(
        &self,
        _repo: RepoId,
        _a: ChangesetId,
        _b: ChangesetId,
    ) -> StoreResult<Option<ChangesetId>> {
        todo!("G1.10")
    }

    /// Line blame of `path` at `at` (lazy, cached).
    pub async fn blame(
        &self,
        _repo: RepoId,
        _at: ChangesetId,
        _path: &RepoPath,
    ) -> StoreResult<Vec<BlameRun>> {
        todo!("G1.10")
    }
}
