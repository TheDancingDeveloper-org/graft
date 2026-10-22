//! The mirror map: graft ids ↔ git shas and the raw headers of imported
//! commits. Owned by the mirror, outside the graft server.

use graft_model::ChangesetId;

use crate::{GitError, GitSha};

/// Raw data kept so imported commits export to their exact sha.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedCommit {
    /// The git commit sha.
    pub sha: GitSha,
    /// The raw commit object bytes, minus tree and parent lines.
    pub raw_headers: Vec<u8>,
}

/// Storage for the mirror map (SQLite in `graft-mirror`).
pub trait MirrorMap: Send + Sync {
    /// The git sha recorded for a changeset.
    fn sha_of(&self, id: ChangesetId) -> Result<Option<GitSha>, GitError>;
    /// The changeset recorded for a git sha.
    fn changeset_of(&self, sha: GitSha) -> Result<Option<ChangesetId>, GitError>;
    /// Records an imported commit.
    fn put_imported(&self, id: ChangesetId, commit: ImportedCommit) -> Result<(), GitError>;
    /// Raw headers of an imported commit.
    fn imported(&self, id: ChangesetId) -> Result<Option<ImportedCommit>, GitError>;
}
