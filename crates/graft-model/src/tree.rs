//! Trees: one directory's entries (`docs/design.md` §4.4).
//!
//! Canonical payload: `u8 version(1)`, then for each entry
//! `kind-byte ‖ u16 len(name) ‖ name ‖ id(32)`, entries sorted by name bytes
//! with names unique. Kind bytes are `f x l d s`.

use crate::changeset::DecodeError;
use crate::codec::{Reader, put_u16};
use crate::id::{ObjectId, TreeId};
use crate::path::check_component;

/// The tree format version byte.
pub const TREE_VERSION: u8 = 1;

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

    /// The kind for a canonical kind byte.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            b'f' => Some(EntryKind::File),
            b'x' => Some(EntryKind::Exec),
            b'l' => Some(EntryKind::Symlink),
            b'd' => Some(EntryKind::Tree),
            b's' => Some(EntryKind::Submodule),
            _ => None,
        }
    }

    /// Whether the entry is a subdirectory.
    pub fn is_tree(self) -> bool {
        self == EntryKind::Tree
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
    /// The canonical payload the tree id hashes. The entries must already be
    /// sorted and unique, as [`Tree::decode`] requires.
    pub fn encode(&self) -> Vec<u8> {
        let size = 1 + self
            .entries
            .iter()
            .map(|e| 3 + e.name.len() + 32)
            .sum::<usize>();
        let mut out = Vec::with_capacity(size);
        out.push(TREE_VERSION);
        for e in &self.entries {
            out.push(e.kind.byte());
            put_u16(
                &mut out,
                u16::try_from(e.name.len()).expect("component fits in u16"),
            );
            out.extend_from_slice(e.name.as_bytes());
            out.extend_from_slice(&e.id.0);
        }
        out
    }

    /// Decodes and validates a canonical payload: known version and kind
    /// bytes, every name a valid path component, names strictly increasing.
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(payload);
        let version = r.u8()?;
        if version != TREE_VERSION {
            return Err(DecodeError::Version(version));
        }
        let mut entries: Vec<TreeEntry> = Vec::new();
        while !r.is_empty() {
            let kind =
                EntryKind::from_byte(r.u8()?).ok_or(DecodeError::NotCanonical("entry-kind"))?;
            let len = r.u16()? as usize;
            let name =
                std::str::from_utf8(r.take(len)?).map_err(|_| DecodeError::NotCanonical("utf8"))?;
            check_component(name).map_err(|_| DecodeError::NotCanonical("entry-name"))?;
            if let Some(prev) = entries.last()
                && prev.name.as_bytes() >= name.as_bytes()
            {
                return Err(DecodeError::NotCanonical("entry-order"));
            }
            let id = r.id()?;
            entries.push(TreeEntry {
                name: name.to_owned(),
                kind,
                id,
            });
        }
        Ok(Tree { entries })
    }

    /// The tree's id.
    pub fn id(&self) -> TreeId {
        TreeId::hash(&self.encode())
    }

    /// Whether the tree has no entries (only the root of an empty
    /// repository may).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Looks up an entry by name.
    pub fn get(&self, name: &str) -> Option<&TreeEntry> {
        self.entries
            .binary_search_by(|e| e.name.as_bytes().cmp(name.as_bytes()))
            .ok()
            .map(|i| &self.entries[i])
    }
}
