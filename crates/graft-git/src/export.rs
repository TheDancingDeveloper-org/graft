//! Deterministic export of graft changesets as git commits.

use graft_model::ChangesetId;
use graft_proto::CallContext;
use graft_proto::api::{ObjectApi, RepoApi};

use crate::map::MirrorMap;
use crate::{GitError, GitSha};

/// Committer identity for native changesets.
#[derive(Clone, Debug)]
pub struct Committer {
    /// Name.
    pub name: String,
    /// Email.
    pub email: String,
}

/// Computes (and records in `map`) the git commit for `changeset`, writing
/// objects into `git_dir`. Repeatable: the same inputs give the same sha.
pub async fn export<A: RepoApi + ObjectApi, M: MirrorMap>(
    _api: &A,
    _cx: &CallContext,
    _map: &M,
    _repo: &str,
    _changeset: ChangesetId,
    _committer: &Committer,
    _git_dir: &std::path::Path,
) -> Result<GitSha, GitError> {
    todo!("G1.8 (sha only, for fidelity), G2.2 (full export)")
}
