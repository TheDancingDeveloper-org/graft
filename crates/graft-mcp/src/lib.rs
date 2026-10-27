//! The graft MCP server (`docs/design.md` §10.1, work items G1.15, G2.8).
//!
//! Written against the API traits, so the same server runs inside
//! `graft mcp` (stdio, remote API, with working-copy tools) and on the
//! server's `/mcp` endpoint (streamable HTTP, in-process engine).

mod drafts;
pub mod preflight;
mod render;
mod review;
mod server;
mod upstream;
mod workcopy;
mod write;

pub use drafts::DraftOverlapArgs;
pub use render::{MAX_RESULT_BYTES, PATCH_MAX_BYTES};
pub use review::{
    AttestArgs, AttestKindArg, AttestStatusArg, GetAttestationsArgs, GetLandChecksArgs,
    ReviewQueueArgs,
};
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
    LandArgs, LineCreateArgs, MergeArg, MergeArgs, PickArgs, ResolveArgs, RevertArgs, SyncArgs,
    TagArgs, WorkspaceAction, WorkspaceArgs,
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
    /// How the result obeys the response bound.
    pub bound: ToolBound,
}

/// A tool result's externally visible bounding strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolBound {
    /// A cursor or offset continues a bounded list or content chunk.
    Paginated,
    /// The result is capped and reports truncation where content is clipped.
    Capped,
}

/// Every tool, in the order of design §10.1.
pub const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "graft_log",
        description: "First-parent history, optionally for paths.",
        read_only: true,
        bound: ToolBound::Paginated,
    },
    ToolSpec {
        name: "graft_show",
        description: "A changeset with its patch.",
        read_only: true,
        bound: ToolBound::Paginated,
    },
    ToolSpec {
        name: "graft_diff",
        description: "Diff two changesets, with capped patches.",
        read_only: true,
        bound: ToolBound::Paginated,
    },
    ToolSpec {
        name: "graft_blame",
        description: "Line origins for a file.",
        read_only: true,
        bound: ToolBound::Paginated,
    },
    ToolSpec {
        name: "graft_read",
        description: "Read a file at a changeset.",
        read_only: true,
        bound: ToolBound::Paginated,
    },
    ToolSpec {
        name: "graft_ls",
        description: "List a directory at a changeset.",
        read_only: true,
        bound: ToolBound::Paginated,
    },
    ToolSpec {
        name: "graft_status",
        description: "Working-copy status (stdio only).",
        read_only: true,
        bound: ToolBound::Capped,
    },
    ToolSpec {
        name: "graft_upstream",
        description: "What changed on the line since the base, and whether your work would merge onto its tip.",
        read_only: true,
        bound: ToolBound::Capped,
    },
    ToolSpec {
        name: "graft_draft_overlap",
        description: "Other live workspaces with open drafts on these paths (no contents). Needs repo:<r>:drafts.",
        read_only: true,
        bound: ToolBound::Capped,
    },
    ToolSpec {
        name: "graft_review_queue",
        description: "Landed changesets that still need review.",
        read_only: true,
        bound: ToolBound::Paginated,
    },
    ToolSpec {
        name: "graft_get_attestations",
        description: "Attestations for a changeset or line range.",
        read_only: true,
        bound: ToolBound::Paginated,
    },
    ToolSpec {
        name: "graft_get_land_checks",
        description: "Check state of landed changesets on a line.",
        read_only: true,
        bound: ToolBound::Paginated,
    },
    ToolSpec {
        name: "graft_attest",
        description: "Record a check, review, or approval outcome.",
        read_only: false,
        bound: ToolBound::Capped,
    },
    ToolSpec {
        name: "graft_commit",
        description: "Create a draft from inline content or the working copy.",
        read_only: false,
        bound: ToolBound::Capped,
    },
    ToolSpec {
        name: "graft_land",
        description: "Land the workspace stack on a line.",
        read_only: false,
        bound: ToolBound::Capped,
    },
    ToolSpec {
        name: "graft_sync",
        description: "Move the workspace base to the line tip.",
        read_only: false,
        bound: ToolBound::Capped,
    },
    ToolSpec {
        name: "graft_revert",
        description: "Create a draft that inverts a changeset.",
        read_only: false,
        bound: ToolBound::Capped,
    },
    ToolSpec {
        name: "graft_pick",
        description: "Create a draft that re-applies a landed changeset.",
        read_only: false,
        bound: ToolBound::Capped,
    },
    ToolSpec {
        name: "graft_conflicts",
        description: "Conflicting paths of a workspace's stack, with markers.",
        read_only: true,
        bound: ToolBound::Capped,
    },
    ToolSpec {
        name: "graft_resolve",
        description: "Rebase the stack onto a target with given resolutions.",
        read_only: false,
        bound: ToolBound::Capped,
    },
    ToolSpec {
        name: "graft_line_create",
        description: "Fork a new line at a changeset; never overwrites.",
        read_only: false,
        bound: ToolBound::Capped,
    },
    ToolSpec {
        name: "graft_merge",
        description: "Merge one line into another, recording a merge point.",
        read_only: false,
        bound: ToolBound::Capped,
    },
    ToolSpec {
        name: "graft_tag",
        description: "Create an immutable tag at a changeset; never moves.",
        read_only: false,
        bound: ToolBound::Capped,
    },
    ToolSpec {
        name: "graft_workspace",
        description: "Open, renew or close a workspace.",
        read_only: false,
        bound: ToolBound::Capped,
    },
];

#[cfg(test)]
mod bound_proofs {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn every_tool_declares_a_bound() {
        let names: BTreeSet<&str> = TOOLS.iter().map(|t| t.name).collect();
        assert_eq!(names.len(), TOOLS.len(), "duplicate tool names");
        for spec in TOOLS {
            assert!(
                matches!(spec.bound, ToolBound::Paginated | ToolBound::Capped),
                "{} has no bound",
                spec.name
            );
        }
    }

    #[test]
    fn the_typed_limit_itself_fits_under_the_cap() {
        let huge = serde_json::json!({"blob": "x".repeat(MAX_RESULT_BYTES)});
        let err = render::bounded(huge).expect_err("over the cap");
        let result = err.into_result();
        assert_eq!(result.is_error, Some(true));
        let value = result.structured_content.as_ref().expect("structured");
        assert_eq!(value["kind"], "result_too_large");
        assert_eq!(value["max_bytes"], MAX_RESULT_BYTES);
        assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_RESULT_BYTES);
    }
}
