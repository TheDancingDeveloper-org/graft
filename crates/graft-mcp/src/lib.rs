//! The graft MCP server (`docs/design.md` §10.1, work items G1.15, G2.8).
//!
//! Written against the API traits, so the same server runs inside
//! `graft mcp` (stdio, remote API, with working-copy tools) and on the
//! server's `/mcp` endpoint (streamable HTTP, in-process engine).

mod drafts;
pub mod preflight;
mod render;
mod server;
mod upstream;
mod workcopy;
mod write;

pub use drafts::DraftOverlapArgs;
pub use render::{MAX_RESULT_BYTES, PATCH_MAX_BYTES};
pub use server::{
    Auth, BlameArgs, Caller, DiffArgs, LogArgs, LsArgs, MAX_REQUEST_BYTES, McpServer, ReadArgs,
    ShowArgs, StatusArgs, http_config, request_auth, streamable_http_service,
};
pub use upstream::UpstreamArgs;
pub use workcopy::{
    WorkingCopyCommit, WorkingCopyError, WorkingCopyFuture, WorkingCopyLand, WorkingCopySync,
    WorkingCopyTools, WorkingCopyUpstream,
};
pub use write::{
    COMMIT_MAX_BYTES, COMMIT_MAX_FILES, COMMIT_MESSAGE_MAX_BYTES, CONFLICT_MARKERS_MAX_BYTES,
    CommitArgs, ConflictsArgs, Encoding, FileChange, FileKind, FileOp, LEASE_MAX_MS, LEASE_MIN_MS,
    LandArgs, LineCreateArgs, MergeArg, MergeArgs, ResolveArgs, RevertArgs, SyncArgs, TagArgs,
    WorkspaceAction, WorkspaceArgs,
};

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
        name: "graft_upstream",
        description: "What changed on the line since the base, and whether your work would merge onto its tip.",
        read_only: true,
    },
    ToolSpec {
        name: "graft_draft_overlap",
        description: "Other live workspaces with open drafts on these paths (no contents). Needs repo:<r>:drafts.",
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
        name: "graft_conflicts",
        description: "Conflicting paths of a workspace's stack, with markers.",
        read_only: true,
    },
    ToolSpec {
        name: "graft_resolve",
        description: "Rebase the stack onto a target with given resolutions.",
        read_only: false,
    },
    ToolSpec {
        name: "graft_line_create",
        description: "Fork a new line at a changeset; never overwrites.",
        read_only: false,
    },
    ToolSpec {
        name: "graft_merge",
        description: "Merge one line into another, recording a merge point.",
        read_only: false,
    },
    ToolSpec {
        name: "graft_tag",
        description: "Create an immutable tag at a changeset; never moves.",
        read_only: false,
    },
    ToolSpec {
        name: "graft_workspace",
        description: "Open, renew or close a workspace.",
        read_only: false,
    },
];
