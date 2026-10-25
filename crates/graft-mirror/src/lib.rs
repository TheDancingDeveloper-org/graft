//! `graft-mirror`: the only service that speaks git (`docs/design.md` §11).
//!
//! Work items G1.14 (import, follow), G3.3 (export),
//! G3.9 (authority switch) and WI-2284 (batch PRs, D21).
//!
//! - [`import`]: a one-shot lossless import of a local git repository, then
//!   the line and tag refs set by compare-and-swap.
//! - [`follow`]: git-primary mode A. Poll upstream with `ls-remote`, fetch
//!   into a local bare mirror, import the new commits, and move each graft
//!   line by admin CAS. A non-fast-forward upstream stops that line and raises
//!   `graft_mirror_divergence`; graft is left unchanged.
//! - [`export`]: graft-primary mode B. Tail the journal, export each line
//!   move, push to GitHub with a lease, and record progress. A refused lease
//!   stops the line and raises `graft_mirror_divergence`; nothing is forced.
//! - [`batch`]: batch mode (decision D21). An integration line forked from
//!   the followed `main` is synced onto it, and its net change goes to
//!   GitHub as one squashed PR per batch, which the operator merges.
//! - [`authority`]: the switch of a line between git-primary and
//!   graft-primary: freeze, verify, flip, unfreeze, in both directions.
//! - [`reconcile`]: an operator resolves a stopped line, landing the stray
//!   commits through graft or replacing them on a lease.
//! - [`pulls`]: GitHub PRs imported as drafts in `github-pr/<n>` workspaces,
//!   landed through graft once approved.
//! - [`land`]: the hand-off to graft's `Land`, and holding imported stacks as
//!   drafts.
//! - [`db`]: the mirror DB (SQLite), implementing `graft-git`'s `MirrorMap`
//!   and holding follow's, export's and batch's per-line state.
//! - [`git`]: the git CLI for `ls-remote`, `fetch` and ancestry.
//! - [`server`]: the graft API over gRPC, including the admin service.
//!
//! The library is generic over the API traits, so tests run it against an
//! in-process engine.

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use graft_git::{GitError, GitSha};
use graft_model::ChangesetId;
use graft_proto::api::RepoApi;
use graft_proto::types::{RefKind, ResolveRef, UpdateRef};
use graft_proto::{ApiError, CallContext};

use crate::github::ForgeError;

pub mod authority;
pub mod batch;
pub mod config;
pub mod credentials;
pub mod db;
pub mod export;
pub mod follow;
pub mod git;
pub mod github;
pub mod import;
pub mod land;
pub mod pulls;
pub mod reconcile;
pub mod server;
pub mod text;

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
    /// GitHub refused or could not be reached.
    Forge(ForgeError),
    /// `git fetch` failed (authentication or the network). The message
    /// names the command and the failure class, never a token or a URL
    /// that carries credentials.
    Fetch(String),
    /// An authority switch step refused, described.
    Switch(String),
    /// The mirror DB has no git commit for a line's tip, and has not
    /// followed the line in this repo: the DB is empty, or belongs to
    /// another repo.
    Unmapped {
        /// The DB file.
        db: PathBuf,
        /// The graft line whose tip is unmapped.
        line: String,
        /// The DB records no commits at all.
        empty: bool,
    },
}

impl fmt::Display for MirrorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MirrorError::Git(e) => e.fmt(f),
            MirrorError::Cli(e) => e.fmt(f),
            MirrorError::Api(e) => write!(f, "the graft API refused: {e:?}"),
            MirrorError::Config(what) => write!(f, "configuration: {what}"),
            MirrorError::Forge(e) => e.fmt(f),
            MirrorError::Fetch(what) => write!(f, "git fetch failed: {what}"),
            MirrorError::Switch(what) => write!(f, "authority switch: {what}"),
            MirrorError::Unmapped {
                db,
                line,
                empty: true,
            } => write!(
                f,
                "mirror DB {} is empty: it has no git commit for line {line}'s tip; \
                 run import or follow for this repo first",
                db.display()
            ),
            MirrorError::Unmapped {
                db,
                line,
                empty: false,
            } => write!(
                f,
                "mirror DB {} has no git commit for line {line}'s tip and has not \
                 followed {line} in this repo; it may be another repo's DB (check \
                 `db` in the config), or the line was never imported",
                db.display()
            ),
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

impl From<ForgeError> for MirrorError {
    fn from(e: ForgeError) -> Self {
        MirrorError::Forge(e)
    }
}

impl From<ApiError> for MirrorError {
    fn from(e: ApiError) -> Self {
        MirrorError::Api(e)
    }
}

/// Where the mirror records which commit it exported for a changeset, so
/// the server can check that a CI job attesting a changeset ran on that
/// changeset's commit (`MirrorService.RecordExports`). Recorded after the
/// commit is written and before it is pushed, so a CI run started by the
/// push always finds it. [`server::Server`] is the real one.
pub trait ExportLog: Send + Sync {
    /// Records each `(changeset, commit)` exported from `repo`.
    fn record_exports<'s>(
        &'s self,
        cx: &'s CallContext,
        repo: &'s str,
        exports: &'s [(ChangesetId, GitSha)],
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 's>>;
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
