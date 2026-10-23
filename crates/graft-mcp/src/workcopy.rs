//! The working-copy hook (`docs/design.md` §10.1, D17): what `graft mcp`
//! adds over stdio when it starts inside a working copy.
//!
//! graft-mcp knows nothing about working copies on disk. The host (the CLI)
//! implements [`WorkingCopyTools`] and hands it to
//! [`McpServer::with_working_copy`](crate::McpServer::with_working_copy).
//! Then `graft_status` answers, `graft_commit` accepts `from_working_copy`,
//! and `graft_land` and `graft_sync` on the working copy's own workspace go
//! through the working copy, so its files and state move with the server.

use std::{fmt, future::Future, pin::Pin};

use graft_proto::{ApiError, CallContext, types::MergeMode};
use serde_json::Value;

/// The future a [`WorkingCopyTools`] method returns.
pub type WorkingCopyFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, WorkingCopyError>> + Send + 'a>>;

/// Why a working-copy operation failed. Messages name paths and ids, never
/// file contents or tokens.
#[derive(Debug)]
#[non_exhaustive]
pub enum WorkingCopyError {
    /// The API returned a typed error.
    Api(ApiError),
    /// The request does not fit this working copy: another repository or
    /// workspace, or an option it does not support.
    Invalid(String),
    /// The working copy could not do it.
    Local {
        /// A short machine-readable kind, such as `nothing_to_commit`,
        /// `diverged` or `conflicted`.
        kind: &'static str,
        /// What happened, for people.
        message: String,
    },
}

impl fmt::Display for WorkingCopyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkingCopyError::Api(e) => write!(f, "{e}"),
            WorkingCopyError::Invalid(m) => f.write_str(m),
            WorkingCopyError::Local { kind, message } => write!(f, "{kind}: {message}"),
        }
    }
}

/// `graft_commit` with `from_working_copy`.
#[derive(Clone, Debug)]
pub struct WorkingCopyCommit {
    /// Repository named in the call.
    pub repo: String,
    /// Workspace named in the call.
    pub workspace: String,
    /// The draft's message.
    pub message: String,
    /// Authoring time in ms since the epoch; now when absent.
    pub authored_at_ms: Option<i64>,
}

/// `graft_land` on the working copy's workspace.
#[derive(Clone, Debug)]
pub struct WorkingCopyLand {
    /// Repository named in the call.
    pub repo: String,
    /// Workspace named in the call.
    pub workspace: String,
    /// `head`, when the call names one.
    pub head: Option<String>,
    /// `line`, when the call names one.
    pub line: Option<String>,
    /// Land the stack as one changeset.
    pub squash: bool,
    /// How the land merges paths both sides changed: the call's `merge`,
    /// a line merge when it gives none.
    pub merge: MergeMode,
}

/// `graft_sync` on the working copy's workspace.
#[derive(Clone, Debug)]
pub struct WorkingCopySync {
    /// Repository named in the call.
    pub repo: String,
    /// Workspace named in the call.
    pub workspace: String,
    /// `onto`, when the call names one.
    pub onto: Option<String>,
}

/// A working copy, as `graft mcp` over stdio serves it.
///
/// Each method returns the tool's JSON result; the server caps it at
/// [`MAX_RESULT_BYTES`](crate::MAX_RESULT_BYTES). `land` and `sync` return
/// `None` when the call names another repository or workspace: the server
/// then serves it through the API like any other.
pub trait WorkingCopyTools: Send + Sync + 'static {
    /// `graft_status`: the line, base, head, workspace and changed paths.
    /// Local; it need not contact the server.
    fn status<'a>(&'a self, repo: &'a str) -> WorkingCopyFuture<'a, Value>;

    /// `graft_commit` with `from_working_copy`: the working copy's changes
    /// as a draft on its workspace.
    fn commit<'a>(
        &'a self,
        cx: &'a CallContext,
        req: WorkingCopyCommit,
    ) -> WorkingCopyFuture<'a, Value>;

    /// `graft_land` for this working copy's workspace, moving the working
    /// copy to what landed; `None` for any other workspace.
    fn land<'a>(
        &'a self,
        cx: &'a CallContext,
        req: WorkingCopyLand,
    ) -> WorkingCopyFuture<'a, Option<Value>>;

    /// `graft_sync` for this working copy's workspace, moving its files;
    /// `None` for any other workspace.
    fn sync<'a>(
        &'a self,
        cx: &'a CallContext,
        req: WorkingCopySync,
    ) -> WorkingCopyFuture<'a, Option<Value>>;
}
