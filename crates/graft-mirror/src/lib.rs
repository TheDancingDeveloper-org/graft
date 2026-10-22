//! `graft-mirror`: the only service that speaks git (`docs/design.md` §11).
//!
//! Work item G1.14 (import, follow); G2.6 (PR bridge), G3.3 (export) and
//! G3.9 (authority switch) come later.
//!
//! - [`import`]: a one-shot lossless import of a local git repository, then
//!   the line and tag refs set by compare-and-swap.
//! - [`follow`]: git-primary mode A. Poll upstream with `ls-remote`, fetch
//!   into a local bare mirror, import the new commits, and move each graft
//!   line by admin CAS. A non-fast-forward upstream stops that line and raises
//!   `graft_mirror_divergence`; graft is left unchanged.
//! - [`db`]: the mirror DB (SQLite), implementing `graft-git`'s `MirrorMap`
//!   and holding follow's per-line state.
//! - [`git`]: the git CLI for `ls-remote`, `fetch` and ancestry.
//! - [`server`]: the graft API over gRPC, including the admin service.
//!
//! The library is generic over the API traits, so tests run it against an
//! in-process engine.

use std::fmt;

use graft_git::GitError;
use graft_model::ChangesetId;
use graft_proto::api::RepoApi;
use graft_proto::types::{RefKind, ResolveRef, UpdateRef};
use graft_proto::{ApiError, CallContext};

pub mod config;
pub mod db;
pub mod follow;
pub mod git;
pub mod import;
pub mod server;

pub use db::MirrorDb;
pub use git::{GitCliError, Mirror};

/// A mirror failure.
#[derive(Debug)]
pub enum MirrorError {
    /// Import or the mirror map failed.
    Git(GitError),
    /// A git command failed.
    Cli(GitCliError),
    /// The graft API refused a call.
    Api(ApiError),
    /// The configuration is unusable, described.
    Config(String),
}

impl fmt::Display for MirrorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MirrorError::Git(e) => e.fmt(f),
            MirrorError::Cli(e) => e.fmt(f),
            MirrorError::Api(e) => write!(f, "the graft API refused: {e:?}"),
            MirrorError::Config(what) => write!(f, "configuration: {what}"),
        }
    }
}

impl std::error::Error for MirrorError {}

impl From<GitError> for MirrorError {
    fn from(e: GitError) -> Self {
        match e {
            GitError::Api(e) => MirrorError::Api(e),
            e => MirrorError::Git(e),
        }
    }
}

impl From<GitCliError> for MirrorError {
    fn from(e: GitCliError) -> Self {
        MirrorError::Cli(e)
    }
}

impl From<ApiError> for MirrorError {
    fn from(e: ApiError) -> Self {
        MirrorError::Api(e)
    }
}

/// The graft ref for a git tag (`refs/tags/<name>`).
pub fn tag_ref(name: &str) -> String {
    format!("tags/{name}")
}

/// What the graft ref named `name` targets now, if it exists.
pub async fn current_ref<A: RepoApi>(
    api: &A,
    cx: &CallContext,
    repo: &str,
    name: &str,
) -> Result<Option<ChangesetId>, ApiError> {
    match api
        .resolve_ref(
            cx,
            ResolveRef {
                repo: repo.to_owned(),
                name: name.to_owned(),
            },
        )
        .await
    {
        Ok(view) => Ok(Some(view.target)),
        Err(ApiError::NotFound { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Moves `name` from `expected` to `new` by compare-and-swap. A no-op when it
/// is already there.
pub async fn move_ref<A: RepoApi>(
    api: &A,
    cx: &CallContext,
    repo: &str,
    name: &str,
    expected: Option<ChangesetId>,
    new: ChangesetId,
    kind: RefKind,
) -> Result<(), ApiError> {
    if expected == Some(new) {
        return Ok(());
    }
    api.update_ref(
        cx,
        UpdateRef {
            repo: repo.to_owned(),
            name: name.to_owned(),
            expected,
            new,
            kind,
        },
    )
    .await
    .map(drop)
}
