//! Import a git repository into graft, verifying fidelity per commit.

use std::path::PathBuf;

use graft_proto::CallContext;
use graft_proto::api::{AdminApi, ObjectApi};

use crate::GitError;
use crate::map::MirrorMap;

/// What to import.
#[derive(Clone, Debug)]
pub struct ImportConfig {
    /// A local bare git repository.
    pub git_dir: PathBuf,
    /// The graft repository.
    pub repo: String,
    /// Git refs to import, mapped to graft lines (`refs/heads/main` → `main`).
    pub lines: Vec<(String, String)>,
    /// Import tags.
    pub tags: bool,
    /// Commits allowed to fail the fidelity check.
    pub allow_lossy: Vec<String>,
    /// Changesets per `ImportChangesets` batch.
    pub batch: usize,
}

/// What an import did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// Commits imported.
    pub commits: u64,
    /// Blobs uploaded.
    pub blobs: u64,
    /// Commits verified by re-export.
    pub verified: u64,
}

/// Imports everything not yet in `map`, oldest first.
pub async fn import<A: AdminApi + ObjectApi, M: MirrorMap>(
    _api: &A,
    _cx: &CallContext,
    _map: &M,
    _config: &ImportConfig,
) -> Result<ImportReport, GitError> {
    todo!("G1.8")
}
