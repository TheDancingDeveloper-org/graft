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
pub mod id;
pub mod merge;
pub mod path;
pub mod time;
pub mod tree;
pub mod tree_ops;

pub use changeset::{Author, AuthorKind, Change, Changeset, Metadata};
pub use id::{BlobId, ChangesetId, IdError, ObjectId, ObjectKind, TreeId};
pub use path::{PathError, RepoPath};
pub use time::{Clock, SystemClock, Timestamp};
pub use tree::{EntryKind, Tree, TreeEntry};
