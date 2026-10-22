//! The graft MCP server (`docs/design.md` §10.1, work items G1.15, G2.8).
//!
//! Written against the API traits, so the same server runs inside
//! `graft mcp` (stdio, remote API, with working-copy tools) and on the
//! server's `/mcp` endpoint (streamable HTTP, in-process engine).

use graft_proto::api::{RepoApi, WorkspaceApi};

/// A tool's identity and hints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolSpec {
    /// Tool name.
    pub name: &'static str,
    /// One-line description shown to agents.
    pub description: &'static str,
    /// Whether the tool only reads (`readOnlyHint`).
    pub read_only: bool,
}

/// Every tool, in the order of design §10.1.
pub const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "graft_log",
        description: "First-parent history, optionally for paths.",
        read_only: true,
    },
    ToolSpec {
        name: "graft_show",
        description: "A changeset with its patch.",
        read_only: true,
    },
    ToolSpec {
        name: "graft_diff",
        description: "Diff two changesets, with capped patches.",
        read_only: true,
    },
    ToolSpec {
        name: "graft_blame",
        description: "Line origins for a file.",
        read_only: true,
    },
    ToolSpec {
        name: "graft_read",
        description: "Read a file at a changeset.",
        read_only: true,
    },
    ToolSpec {
        name: "graft_ls",
        description: "List a directory at a changeset.",
        read_only: true,
    },
    ToolSpec {
        name: "graft_status",
        description: "Working-copy status (stdio only).",
        read_only: true,
    },
    ToolSpec {
        name: "graft_commit",
        description: "Create a draft from inline content or the working copy.",
        read_only: false,
    },
    ToolSpec {
        name: "graft_land",
        description: "Land the workspace stack on a line.",
        read_only: false,
    },
    ToolSpec {
        name: "graft_sync",
        description: "Move the workspace base to the line tip.",
        read_only: false,
    },
    ToolSpec {
        name: "graft_revert",
        description: "Create a draft that inverts a changeset.",
        read_only: false,
    },
    ToolSpec {
        name: "graft_workspace",
        description: "Open, renew or close a workspace.",
        read_only: false,
    },
];

/// An MCP server over an API implementation `A`.
#[derive(Debug)]
pub struct McpServer<A> {
    api: A,
}

impl<A: RepoApi + WorkspaceApi> McpServer<A> {
    /// Wraps an API.
    pub fn new(api: A) -> Self {
        Self { api }
    }

    /// The API in use.
    pub fn api(&self) -> &A {
        &self.api
    }

    /// Serves MCP over stdin and stdout until EOF.
    pub async fn serve_stdio(&self) -> std::io::Result<()> {
        todo!("G1.15")
    }
}
