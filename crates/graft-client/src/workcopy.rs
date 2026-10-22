//! The working copy (`docs/design.md` §9): `.graft/` state, sparse prefixes,
//! stat-based status with no staging area.

use std::path::{Path, PathBuf};

use graft_model::{ChangesetId, RepoPath};

/// The persisted working-copy state (`.graft/state`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WcState {
    /// Repository.
    pub repo: String,
    /// Workspace name.
    pub workspace: String,
    /// Line tracked.
    pub line: String,
    /// Base changeset.
    pub base: ChangesetId,
    /// Head draft.
    pub head: Option<ChangesetId>,
}

/// Sparse prefixes (`.graft/sparse`). Empty means the whole snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sparse {
    /// Included prefixes.
    pub prefixes: Vec<RepoPath>,
    /// Include files at the repository root.
    pub root_files: bool,
}

/// One file's status against the head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileStatus {
    /// New file in the slice.
    Added(RepoPath),
    /// Content or mode changed.
    Modified(RepoPath),
    /// Removed from the working copy.
    Deleted(RepoPath),
}

/// A working copy on disk.
#[derive(Debug)]
pub struct WorkingCopy {
    root: PathBuf,
}

impl WorkingCopy {
    /// Opens the working copy containing `path`.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            root: path.to_owned(),
        })
    }

    /// The working copy root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Reads `.graft/state`.
    pub fn state(&self) -> std::io::Result<WcState> {
        todo!("G1.11")
    }

    /// Reads `.graft/sparse`.
    pub fn sparse(&self) -> std::io::Result<Sparse> {
        todo!("G1.11")
    }

    /// Status against the head, re-hashing only on a stat mismatch or a
    /// racily clean entry.
    pub fn status(&self) -> std::io::Result<Vec<FileStatus>> {
        todo!("G1.11")
    }
}
