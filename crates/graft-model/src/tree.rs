//! Trees: one directory's entries (`docs/design.md` §4.4).

use crate::id::ObjectId;

/// The kind of a tree entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// An executable file.
    Exec,
    /// A symbolic link; the blob holds the target.
    Symlink,
    /// A subdirectory.
    Tree,
    /// An opaque external pointer, kept for lossless import.
    Submodule,
}

impl EntryKind {
    /// The canonical kind byte (`f x l d s`).
    pub fn byte(self) -> u8 {
        match self {
            EntryKind::File => b'f',
            EntryKind::Exec => b'x',
            EntryKind::Symlink => b'l',
            EntryKind::Tree => b'd',
            EntryKind::Submodule => b's',
        }
    }
}

/// One entry in a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    /// A single valid path component.
    pub name: String,
    /// What the entry is.
    pub kind: EntryKind,
    /// Blob id, tree id, or an opaque submodule reference.
    pub id: ObjectId,
}

/// A directory: entries sorted by name bytes, names unique.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    /// The entries, sorted by name bytes.
    pub entries: Vec<TreeEntry>,
}

impl Tree {
    /// The canonical payload the tree id hashes.
    pub fn encode(&self) -> Vec<u8> {
        todo!("G1.1: canonical tree encoding")
    }

    /// Decodes and validates a canonical payload.
    pub fn decode(_payload: &[u8]) -> Result<Self, crate::changeset::DecodeError> {
        todo!("G1.1: canonical tree decoding")
    }

    /// Looks up an entry by name.
    pub fn get(&self, name: &str) -> Option<&TreeEntry> {
        self.entries
            .binary_search_by(|e| e.name.as_bytes().cmp(name.as_bytes()))
            .ok()
            .map(|i| &self.entries[i])
    }
}
