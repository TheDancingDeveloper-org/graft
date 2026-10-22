//! The graft LSP server (`docs/design.md` §10.2, work item G3.6).
//!
//! Runs next to the editor's language server and adds source-control
//! features: blame hovers, history code lenses, conflict and staleness
//! diagnostics, commands, and `graft:` virtual documents.

use graft_client::workcopy::WorkingCopy;
use graft_proto::api::{RepoApi, WorkspaceApi};

/// Custom LSP request for reading a file at a changeset.
pub const READ_FILE_REQUEST: &str = "graft/readFile";

/// Commands offered through `workspace/executeCommand`.
pub const COMMANDS: &[&str] = &[
    "graft.commit",
    "graft.land",
    "graft.sync",
    "graft.diffBase",
    "graft.blameFile",
];

/// An LSP server over an API and an optional working copy.
#[derive(Debug)]
pub struct LspServer<A> {
    api: A,
    workcopy: Option<WorkingCopy>,
}

impl<A: RepoApi + WorkspaceApi> LspServer<A> {
    /// Builds a server.
    pub fn new(api: A, workcopy: Option<WorkingCopy>) -> Self {
        Self { api, workcopy }
    }

    /// The API in use.
    pub fn api(&self) -> &A {
        &self.api
    }

    /// The working copy, if any.
    pub fn workcopy(&self) -> Option<&WorkingCopy> {
        self.workcopy.as_ref()
    }

    /// Serves LSP over stdin and stdout until shutdown.
    pub async fn serve_stdio(&self) -> std::io::Result<()> {
        todo!("G3.6")
    }
}
