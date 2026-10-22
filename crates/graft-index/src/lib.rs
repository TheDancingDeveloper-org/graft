//! Derived data and history queries (`docs/design.md` §4.9, §7.5, §7.7).
//!
//! Work items G1.4 (framework and eager kinds) and G1.10 (queries). Eager
//! kinds produce [`graft_store::Write`] rows that the engine commits in the
//! same transaction as the write they derive from.

use graft_model::{Changeset, ChangesetId, RepoPath};
use graft_store::{MetaStore, ObjectStore, RepoId, StoreResult, Write};

mod ancestry;
mod blame;
mod derive;
mod history;
pub mod kinds;
mod line;

pub use derive::{Derived, row};
pub use line::{line_path_rows, path_prefixes};

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

/// A kind whose values live in the derived table, keyed by
/// `(NAME, VERSION, key)`. The encoding is a durable format.
pub trait ValueKind: DerivedKind {
    /// The decoded value.
    type Value: Clone;
    /// The stored bytes.
    fn encode(value: &Self::Value) -> Vec<u8>;
    /// The value, or `None` when the bytes are malformed.
    fn decode(bytes: &[u8]) -> Option<Self::Value>;
}

/// A per-changeset kind computed from the changeset and its needed parents'
/// values only: `derive(c) = f(c, [derive(p) for p in needed_parents(c)])`.
/// The framework walks and caches it ([`Index::derive`]).
pub trait ParentDerived: ValueKind {
    /// `f`. `parents` holds one value per needed parent, in parent order.
    fn derive(cs: &Changeset, parents: &[Self::Value]) -> Self::Value;
}

/// The parents a kind of this scope reads.
pub fn needed_parents(scope: ParentScope, cs: &Changeset) -> &[ChangesetId] {
    match scope {
        ParentScope::First => &cs.parents[..cs.parents.len().min(1)],
        ParentScope::All => &cs.parents,
    }
}

/// The derived-table key of a per-changeset value: the 32 id bytes.
pub fn changeset_key(id: ChangesetId) -> Vec<u8> {
    id.0.0.to_vec()
}

/// A history query (the engine maps `graft_proto::types::Log` onto it).
///
/// To page, ask again with `from` set to the last result's first parent
/// (any result), or keep `from` and set `after_pos` to the last result's
/// line position (a result on the line).
#[derive(Clone, Debug)]
pub struct LogQuery {
    /// Newest changeset.
    pub from: ChangesetId,
    /// Exclusive stop: neither it nor, once the walk is on its line,
    /// anything at or below its position there is returned.
    pub until: Option<ChangesetId>,
    /// Path or directory filters (any may match); empty means every
    /// changeset.
    pub paths: Vec<RepoPath>,
    /// Maximum results, clamped to `1..=MAX_PAGE`.
    pub limit: u32,
    /// Resume after this line position: skip the stretch of drafts above
    /// the line and return only line positions below it.
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

    /// Eager rows for a newly created changeset (generation, skip list),
    /// plus rows for any ancestors whose values were missing (after a
    /// version bump). Reads are O(parents + log depth) when the parents'
    /// values are present.
    pub async fn on_create(&self, repo: RepoId, cs: &Changeset) -> StoreResult<Vec<Write>> {
        let id = cs.id();
        let generation = self.derive_new::<kinds::Generation>(repo, id, cs).await?;
        let skips = self.skip_list_new(repo, id, cs).await?;
        let mut rows = generation.rows;
        rows.extend(skips.rows);
        Ok(rows)
    }

    /// Eager rows for changesets landing on `line` (positions, path index).
    ///
    /// `landed` is the first-parent chain being added, oldest first: each
    /// changeset's first parent is the one before it, and the first one's
    /// first parent is the line's current target (or it is a root, which
    /// takes position 0). One read: the position of that first parent.
    pub async fn on_land(
        &self,
        repo: RepoId,
        line: &str,
        landed: &[Changeset],
    ) -> StoreResult<Vec<Write>> {
        let Some(first) = landed.first() else {
            return Ok(Vec::new());
        };
        let start = match first.first_parent() {
            None => 0,
            Some(parent) => {
                let pos = self
                    .meta
                    .line_pos(repo, line, parent)
                    .await?
                    .ok_or_else(|| {
                        graft_store::StoreError::Invalid(format!("{parent} is not on line {line}"))
                    })?;
                pos + 1
            }
        };
        let mut rows = Vec::new();
        for (pos, cs) in (start..).zip(landed) {
            debug_assert!(pos == start || cs.first_parent().is_some());
            let id = cs.id();
            rows.push(Write::PutLinePos {
                line: line.to_owned(),
                changeset: id,
                pos,
            });
            rows.extend(line_path_rows(line, pos, id, cs));
        }
        Ok(rows)
    }
}
