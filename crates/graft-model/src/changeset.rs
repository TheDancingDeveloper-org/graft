//! Changesets: the unit of work (`docs/design.md` §4.5).

use std::collections::BTreeMap;

use crate::id::{ChangesetId, ObjectId, TreeId};
use crate::path::RepoPath;
use crate::time::Timestamp;
use crate::tree::EntryKind;

/// Sorted string map for tools. Keys under `graft.` are reserved.
pub type Metadata = BTreeMap<String, String>;

/// Who wrote a changeset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AuthorKind {
    /// A person.
    Human,
    /// An AI agent session.
    Agent,
    /// An automated tool (revert bot, dependency updater).
    Bot,
    /// A CI job.
    Ci,
}

/// The author of a changeset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Author {
    /// Display name.
    pub name: String,
    /// Email or other contact.
    pub email: String,
    /// Human, agent, bot or CI.
    pub kind: AuthorKind,
}

/// One file-level change relative to the first parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Create or replace the entry at `path`. `kind` is never `Tree`.
    Put {
        /// Where.
        path: RepoPath,
        /// File, exec, symlink or submodule.
        kind: EntryKind,
        /// The blob (or submodule reference).
        id: ObjectId,
    },
    /// Remove the entry at `path`.
    Delete {
        /// Where.
        path: RepoPath,
    },
}

impl Change {
    /// The path this change touches.
    pub fn path(&self) -> &RepoPath {
        match self {
            Change::Put { path, .. } | Change::Delete { path } => path,
        }
    }
}

/// A changeset. Every field is part of its id; nothing server-assigned is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changeset {
    /// Ordered parents; empty only for a root.
    pub parents: Vec<ChangesetId>,
    /// The root tree of the resulting snapshot.
    pub root: TreeId,
    /// Changes against `parents[0]`, sorted by path.
    pub changes: Vec<Change>,
    /// Who wrote it.
    pub author: Author,
    /// When, as supplied by the client.
    pub authored_at: Timestamp,
    /// Description.
    pub message: String,
    /// Tool metadata.
    pub metadata: Metadata,
}

impl Changeset {
    /// The canonical payload the changeset id hashes.
    pub fn encode(&self) -> Vec<u8> {
        todo!("G1.1: canonical changeset encoding")
    }

    /// Decodes and validates a canonical payload.
    pub fn decode(_payload: &[u8]) -> Result<Self, DecodeError> {
        todo!("G1.1: canonical changeset decoding")
    }

    /// The changeset's id.
    pub fn id(&self) -> ChangesetId {
        ChangesetId(ObjectId::hash(
            crate::id::ObjectKind::Changeset,
            &self.encode(),
        ))
    }

    /// The first parent, if any.
    pub fn first_parent(&self) -> Option<ChangesetId> {
        self.parents.first().copied()
    }
}

/// Why a canonical payload failed to decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Unknown format version byte.
    Version(u8),
    /// The payload ended early.
    Truncated,
    /// Bytes left over after the last field.
    TrailingBytes,
    /// A field broke a canonical-form rule (ordering, uniqueness, UTF-8).
    NotCanonical(&'static str),
}
