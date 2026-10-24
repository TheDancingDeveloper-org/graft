//! The mirror map: graft ids ↔ git shas, the raw headers of imported commits,
//! the raw tag objects of imported tags, and the raw git trees graft's model
//! cannot express. Owned by the mirror, outside the graft server
//! (`docs/design.md` §11).

use std::collections::HashMap;
use std::sync::Mutex;

use graft_model::{BlobId, ChangesetId, RepoPath};

use crate::{GitError, GitSha};

/// A directory whose git tree the export cannot derive from graft's
/// snapshot, recorded per commit: a legacy mode such as `100664`, an empty
/// subtree, entries out of git's order, or a mode git changed while graft's
/// kind stayed the same.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawTree {
    /// The directory; `None` is the root.
    pub dir: Option<RepoPath>,
    /// The raw git tree object, or `None` when git has no directory there
    /// although the path-copied tree would still hold entries (only
    /// non-canonical leftovers such as empty subtrees).
    pub tree: Option<Vec<u8>>,
}

/// What the export needs, beyond the changeset, to reproduce an imported
/// commit's sha exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedCommit {
    /// The git commit sha.
    pub sha: GitSha,
    /// The raw header lines after the tree and parent lines (author,
    /// committer, encoding, gpgsig, mergetag and any extra headers), through
    /// the blank line that ends them.
    pub headers: Vec<u8>,
    /// The raw message, when it differs from the changeset's message (which
    /// is the message decoded as UTF-8, with invalid bytes replaced).
    pub message: Option<Vec<u8>>,
    /// Directories whose git tree is not the path-copied one, deepest
    /// first.
    pub raw_trees: Vec<RawTree>,
}

/// An imported annotated (or signed, or nested) tag: the raw tag objects its
/// ref peels through, so the export reproduces the ref's sha. Lightweight
/// tags have no record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedTag {
    /// The commit the chain ends at.
    pub commit: GitSha,
    /// The raw tag objects, outermost (the one the ref names) first.
    pub objects: Vec<Vec<u8>>,
}

/// Storage for the mirror map (SQLite in `graft-mirror`).
pub trait MirrorMap: Send + Sync {
    /// The git sha recorded for a changeset.
    fn sha_of(&self, id: ChangesetId) -> Result<Option<GitSha>, GitError>;
    /// The changeset recorded for a git sha.
    fn changeset_of(&self, sha: GitSha) -> Result<Option<ChangesetId>, GitError>;
    /// Records an imported commit.
    fn put_imported(&self, id: ChangesetId, commit: ImportedCommit) -> Result<(), GitError>;
    /// The record of an imported commit.
    fn imported(&self, id: ChangesetId) -> Result<Option<ImportedCommit>, GitError>;
    /// Records the tag objects of an imported tag (name without
    /// `refs/tags/`), replacing any earlier record.
    fn put_tag(&self, name: &str, tag: ImportedTag) -> Result<(), GitError>;
    /// The record of an imported tag.
    fn tag(&self, name: &str) -> Result<Option<ImportedTag>, GitError>;
    /// Records the git sha of blobs (the export's blob sha cache).
    fn put_blob_shas(&self, pairs: &[(BlobId, GitSha)]) -> Result<(), GitError>;
    /// The git sha recorded for a blob.
    fn blob_sha(&self, id: BlobId) -> Result<Option<GitSha>, GitError>;
}

#[derive(Default)]
struct Maps {
    by_changeset: HashMap<ChangesetId, ImportedCommit>,
    by_sha: HashMap<GitSha, ChangesetId>,
    blobs: HashMap<BlobId, GitSha>,
    tags: HashMap<String, ImportedTag>,
}

/// An in-memory [`MirrorMap`], for tests and one-shot tools.
#[derive(Default)]
pub struct MemoryMirrorMap {
    maps: Mutex<Maps>,
}

impl MemoryMirrorMap {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of imported commits recorded.
    pub fn len(&self) -> usize {
        self.lock().by_changeset.len()
    }

    /// Whether no commit is recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Maps> {
        self.maps.lock().expect("mirror map lock")
    }
}

impl MirrorMap for MemoryMirrorMap {
    fn sha_of(&self, id: ChangesetId) -> Result<Option<GitSha>, GitError> {
        Ok(self.lock().by_changeset.get(&id).map(|c| c.sha))
    }

    fn changeset_of(&self, sha: GitSha) -> Result<Option<ChangesetId>, GitError> {
        Ok(self.lock().by_sha.get(&sha).copied())
    }

    fn put_imported(&self, id: ChangesetId, commit: ImportedCommit) -> Result<(), GitError> {
        let mut maps = self.lock();
        maps.by_sha.insert(commit.sha, id);
        maps.by_changeset.insert(id, commit);
        Ok(())
    }

    fn imported(&self, id: ChangesetId) -> Result<Option<ImportedCommit>, GitError> {
        Ok(self.lock().by_changeset.get(&id).cloned())
    }

    fn put_tag(&self, name: &str, tag: ImportedTag) -> Result<(), GitError> {
        self.lock().tags.insert(name.to_owned(), tag);
        Ok(())
    }

    fn tag(&self, name: &str) -> Result<Option<ImportedTag>, GitError> {
        Ok(self.lock().tags.get(name).cloned())
    }

    fn put_blob_shas(&self, pairs: &[(BlobId, GitSha)]) -> Result<(), GitError> {
        self.lock().blobs.extend(pairs.iter().copied());
        Ok(())
    }

    fn blob_sha(&self, id: BlobId) -> Result<Option<GitSha>, GitError> {
        Ok(self.lock().blobs.get(&id).copied())
    }
}
