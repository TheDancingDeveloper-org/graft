//! graft's content-addressed data model.
//!
//! This crate holds the types every other crate names (ids, paths, blobs,
//! trees, changesets) with their canonical encodings, plus pure algorithms on
//! trees (`apply_changes`, `diff_trees`) and text (`merge3`). It has no I/O and
//! no transport: algorithms read objects through [`tree_ops::ObjectSource`].
//!
//! The durable formats are specified in `docs/design.md` §4 and are pinned by
//! golden vectors (work item G1.1).

pub mod changeset;
mod codec;
pub mod id;
pub mod merge;
pub mod path;
pub mod time;
pub mod tree;
pub mod tree_ops;

pub use changeset::{
    Author, AuthorKind, Change, Changeset, CheckOptions, DecodeError, Metadata, Rule, Violation,
};
pub use id::{BlobId, ChangesetId, IdError, IdPrefix, ObjectId, ObjectKind, TreeId};
pub use path::{PathError, Portability, RepoPath};
pub use time::{Clock, SystemClock, Timestamp};
pub use tree::{EntryKind, Tree, TreeEntry};
pub use tree_ops::{
    Applied, ApplyError, ApplyOptions, DiffPage, ObjectSource, SourceError, apply_changes,
    apply_changes_with, diff_trees, diff_trees_after,
};
