//! The write tools (`docs/design.md` §10.1, work item G2.8): commit inline
//! content, sync, land (the PR bridge on git-primary lines) and workspaces.
//!
//! Each tool maps to API operations called as the request's caller, so
//! scopes, policies and idempotency are the API's own. Arguments are checked
//! here first (names, paths, sizes) and every result is bounded. Contents and
//! messages are stored and returned as data; nothing here interprets them.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use graft_model::{
    Author, AuthorKind, BlobId, Change, ChangesetId, EntryKind, Metadata, ObjectKind, RepoPath,
    Timestamp,
};
use graft_proto::{
    ApiError, CallContext, Frame, PrincipalKind,
    api::{RepoApi, WorkspaceApi},
    types::{
        ConflictReason, ConflictSide, ConflictView, CreateChangeset, GetConflicts, Land, Landed,
        MergeMode, OpenWorkspace, Page, PathResolution, Resolution, ResolvedContent, Revert,
        WorkspaceRef, WorkspaceView,
    },
};
use rmcp::{
    ErrorData, RoleServer, handler::server::wrapper::Parameters, model::CallToolResult,
    service::RequestContext, tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::render::{FIELD_MAX_BYTES, ToolError, bounded, clip, fit};
use crate::server::{McpServer, kind_name, parse_path, respond, state_name};
use crate::workcopy::{WorkingCopyCommit, WorkingCopyLand, WorkingCopySync};

/// Most files one inline commit names.
pub const COMMIT_MAX_FILES: usize = 256;
/// Most content bytes (decoded) one inline commit carries. Larger changes
/// go through a working copy.
pub const COMMIT_MAX_BYTES: u64 = 1 << 20;
/// Longest commit message accepted, in bytes.
pub const COMMIT_MESSAGE_MAX_BYTES: usize = 64 * 1024;
/// Workspace lease bounds and default, in milliseconds.
pub const LEASE_MIN_MS: u64 = 60_000;
/// The longest lease one call asks for.
pub const LEASE_MAX_MS: u64 = 24 * 60 * 60 * 1000;
const LEASE_DEFAULT_MS: u64 = 60 * 60 * 1000;
/// Paths listed in a sync or land result before it says `truncated`.
const PATHS_MAX: usize = 200;
/// A pull request URL longer than this is clipped.
const URL_MAX_BYTES: usize = 1024;
/// Conflicting paths one `graft_conflicts` page asks for, by default and at
/// most. The page shrinks further when the markers do not fit.
const CONFLICTS_DEFAULT: u32 = 16;
const CONFLICTS_MAX: u32 = 64;
/// Marker text kept per path in a `graft_conflicts` result, in bytes.
pub const CONFLICT_MARKERS_MAX_BYTES: usize = 16 * 1024;
/// The working-copy metadata directory, never staged.
const RESERVED_DIR: &str = ".graft";

/// What `graft_commit` does with one path.
#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileOp {
    /// Write `content` at the path.
    #[default]
    Put,
    /// Remove the path.
    Delete,
}

/// How a file's `content` is encoded.
#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    /// The bytes are the UTF-8 text itself.
    #[default]
    Utf8,
    /// Two lowercase or uppercase hex digits per byte.
    Hex,
}

/// The entry kind of a written file.
#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    /// A regular file.
    #[default]
    File,
    /// An executable file.
    Exec,
    /// A symbolic link; the content is its target.
    Symlink,
}

/// One file of an inline commit.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileChange {
    /// The path, relative to the repository root, `/`-separated. `.` and
    /// `..` components and the `.graft` directory are refused.
    pub path: String,
    /// `put` (default) writes `content`; `delete` removes the path.
    pub op: Option<FileOp>,
    /// The new content, for `put`.
    pub content: Option<String>,
    /// How `content` is encoded: `utf8` (default) or `hex`.
    pub encoding: Option<Encoding>,
    /// Entry kind for `put`: `file` (default), `exec` or `symlink`.
    pub kind: Option<FileKind>,
}

/// Arguments of `graft_commit`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitArgs {
    /// Repository name.
    pub repo: String,
    /// The workspace whose stack the draft goes on.
    pub workspace: String,
    /// The commit message.
    pub message: String,
    /// The changed files, with inline content (at most 256 files and 1 MiB
    /// of content).
    #[serde(default)]
    pub files: Vec<FileChange>,
    /// Commit the working copy's changes instead of `files`. Only `graft mcp`
    /// over stdio, inside a working copy, has one.
    pub from_working_copy: Option<bool>,
    /// The parent changeset (an id or a ref name). Defaults to the workspace
    /// head, or its base when it has no drafts. To retry a commit without
    /// making a second draft, pass the `parent` and `authored_at_ms` the
    /// first attempt returned.
    pub parent: Option<String>,
    /// Authoring time, ms since the epoch. Defaults to now.
    pub authored_at_ms: Option<i64>,
}

/// Arguments of `graft_sync`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SyncArgs {
    /// Repository name.
    pub repo: String,
    /// The workspace to rebase.
    pub workspace: String,
    /// Rebase onto this changeset (an id or a ref name) instead of the line
    /// tip.
    pub onto: Option<String>,
}

/// How land merges paths both sides changed.
#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MergeArg {
    /// Any path changed on both sides conflicts.
    File,
    /// Three-way line merge for text files (the default): changes to
    /// different regions of one file merge; overlapping changes conflict.
    #[default]
    Line,
}

/// Arguments of `graft_land`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LandArgs {
    /// Repository name.
    pub repo: String,
    /// The workspace whose stack lands.
    pub workspace: String,
    /// The stack head to land (an id or a ref name). Defaults to the
    /// workspace head.
    pub head: Option<String>,
    /// The target line. Defaults to the line the workspace tracks.
    pub line: Option<String>,
    /// Land the stack as one changeset (default false).
    pub squash: Option<bool>,
    /// `line` (default): a three-way line merge, so changes to different
    /// regions of one file land cleanly. `file`: any path changed on both
    /// sides conflicts.
    #[schemars(extend("default" = "line"))]
    pub merge: Option<MergeArg>,
}

/// What `graft_workspace` does.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceAction {
    /// Open a workspace on `line`.
    Open,
    /// Read a workspace.
    Get,
    /// Extend a workspace's lease.
    Renew,
    /// Close a workspace.
    Close,
}

/// Arguments of `graft_workspace`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceArgs {
    /// Repository name.
    pub repo: String,
    /// `open`, `get`, `renew` or `close`.
    pub action: WorkspaceAction,
    /// The workspace. Required except for `open`, where the server picks a
    /// name when it is absent.
    pub name: Option<String>,
    /// `open` only: the line to track.
    pub line: Option<String>,
    /// `open` only: the base (an id or a ref name); the line tip when absent.
    pub at: Option<String>,
    /// `open` and `renew`: lease length in ms (60,000 to 86,400,000; default
    /// one hour).
    pub lease_ms: Option<u64>,
    /// `close` only: keep the drafts for later (default true).
    pub keep_drafts: Option<bool>,
}

/// Arguments of `graft_conflicts`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConflictsArgs {
    /// Repository name.
    pub repo: String,
    /// The workspace whose stack would be rebased.
    pub workspace: String,
    /// The target (an id or a ref name); the line tip when absent.
    pub onto: Option<String>,
    /// Conflicting paths per page (1 to 64, default 16).
    pub limit: Option<u32>,
    /// Resume token from a previous result's `next`.
    pub cursor: Option<String>,
}

/// Arguments of `graft_resolve`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveArgs {
    /// Repository name.
    pub repo: String,
    /// The workspace whose stack is rebased.
    pub workspace: String,
    /// The target, as `graft_conflicts` or a `conflict` error returned it.
    pub onto: String,
    /// The workspace head the caller saw (compare-and-swap).
    pub expected_head: Option<String>,
    /// One entry per conflicting path: `put` (default) with `content`, or
    /// `delete`. The same shape and limits as `graft_commit`'s `files`.
    pub resolutions: Vec<FileChange>,
}

/// Arguments of `graft_revert`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RevertArgs {
    /// Repository name.
    pub repo: String,
    /// The changeset to invert (an id or a ref name).
    pub changeset: String,
    /// Where to apply the inverse (an id or a ref name). Defaults to the
    /// workspace head, or its base when it has no drafts.
    pub onto: Option<String>,
    /// The workspace to hold the new draft.
    pub workspace: Option<String>,
}

impl<A: RepoApi + WorkspaceApi + Send + Sync + 'static> McpServer<A> {
    async fn conflicts(
        &self,
        cx: &CallContext,
        args: ConflictsArgs,
    ) -> Result<CallToolResult, ToolError> {
        check_name("repo", &args.repo)?;
        check_name("workspace", &args.workspace)?;
        let onto = match &args.onto {
            Some(rev) => Some(self.resolve(cx, &args.repo, rev).await?),
            None => None,
        };
        let size = match args.limit.unwrap_or(CONFLICTS_DEFAULT) {
            n if (1..=CONFLICTS_MAX).contains(&n) => n,
            n => {
                return Err(invalid(&format!(
                    "limit {n} is outside 1 to {CONFLICTS_MAX}"
                )));
            }
        };
        let workspace = ws_ref(&args.repo, &args.workspace);
        fit(
            size,
            |limit| {
                let req = GetConflicts {
                    workspace: workspace.clone(),
                    onto,
                    page: Page {
                        limit,
                        token: args.cursor.clone(),
                    },
                };
                self.api.get_conflicts(cx, req)
            },
            |c, _| {
                json!({
                    "onto": c.onto.to_string(),
                    "draft": c.draft.map(|d| d.to_string()),
                    "paths": c.paths.iter().map(conflict_json).collect::<Vec<_>>(),
                    "next": c.next,
                })
            },
        )
        .await
    }

    async fn resolve_stack(
        &self,
        cx: &CallContext,
        args: ResolveArgs,
    ) -> Result<CallToolResult, ToolError> {
        check_name("repo", &args.repo)?;
        check_name("workspace", &args.workspace)?;
        if args.resolutions.is_empty() {
            return Err(invalid("`resolutions` names no paths"));
        }
        let resolutions = resolutions(&args.resolutions)?;
        let onto = self.resolve(cx, &args.repo, &args.onto).await?;
        let expected_head = match &args.expected_head {
            Some(rev) => Some(self.resolve(cx, &args.repo, rev).await?),
            None => None,
        };
        let req = graft_proto::types::Resolve {
            workspace: ws_ref(&args.repo, &args.workspace),
            onto,
            expected_head,
            resolutions,
        };
        let resolved = self.api.resolve(cx, req).await.map_err(ToolError::Api)?;
        let mut value = json!({
            "workspace": workspace_json(&resolved.workspace),
            "head": resolved.head.to_string(),
        });
        put_paths(&mut value, "resolved_paths", &resolved.resolved_paths);
        put_paths(&mut value, "merged_paths", &resolved.merged_paths);
        bounded(value)
    }

    async fn revert(
        &self,
        cx: &CallContext,
        args: RevertArgs,
    ) -> Result<CallToolResult, ToolError> {
        check_name("repo", &args.repo)?;
        if let Some(ws) = &args.workspace {
            check_name("workspace", ws)?;
        }
        let changeset = self.resolve(cx, &args.repo, &args.changeset).await?;
        let onto = match (&args.onto, &args.workspace) {
            (Some(rev), _) => self.resolve(cx, &args.repo, rev).await?,
            (None, Some(ws)) => {
                let ws = self.workspace(cx, &args.repo, ws).await?;
                ws.head.unwrap_or(ws.base)
            }
            (None, None) => return Err(invalid("give `onto` or `workspace`")),
        };
        let req = Revert {
            repo: args.repo,
            changeset,
            onto,
            workspace: args.workspace.clone(),
        };
        let id = self.api.revert(cx, req).await.map_err(ToolError::Api)?;
        bounded(json!({
            "id": id.to_string(),
            "reverted": changeset.to_string(),
            "onto": onto.to_string(),
            "workspace": args.workspace.as_deref().map(|w| clip(w, FIELD_MAX_BYTES).0),
        }))
    }

    async fn commit(
        &self,
        cx: &CallContext,
        args: CommitArgs,
    ) -> Result<CallToolResult, ToolError> {
        check_name("repo", &args.repo)?;
        check_name("workspace", &args.workspace)?;
        if args.from_working_copy == Some(true) {
            if !args.files.is_empty() {
                return Err(invalid("give `files` or `from_working_copy`, not both"));
            }
            let Some(wc) = &self.working_copy else {
                return Err(ToolError::NeedsWorkingCopy {
                    tool: "graft_commit",
                });
            };
            if args.parent.is_some() {
                return Err(invalid(
                    "`parent` does not apply to `from_working_copy`: the draft goes on the workspace head",
                ));
            }
            if args.message.trim().is_empty() {
                return Err(invalid("`message` is empty"));
            }
            if args.message.len() > COMMIT_MESSAGE_MAX_BYTES {
                return Err(ToolError::Api(ApiError::LimitExceeded {
                    limit: "message-bytes".into(),
                    max: COMMIT_MESSAGE_MAX_BYTES as u64,
                }));
            }
            let req = WorkingCopyCommit {
                repo: args.repo,
                workspace: args.workspace,
                message: args.message,
                authored_at_ms: args.authored_at_ms,
            };
            return bounded(wc.commit(cx, req).await?);
        }
        if args.files.is_empty() {
            return Err(invalid("`files` names no changes"));
        }
        if args.message.trim().is_empty() {
            return Err(invalid("`message` is empty"));
        }
        if args.message.len() > COMMIT_MESSAGE_MAX_BYTES {
            return Err(ToolError::Api(ApiError::LimitExceeded {
                limit: "message-bytes".into(),
                max: COMMIT_MESSAGE_MAX_BYTES as u64,
            }));
        }
        let (changes, inline_blobs) = stage(&args.files)?;
        let parent = match &args.parent {
            Some(rev) => self.resolve(cx, &args.repo, rev).await?,
            None => {
                let ws = self.workspace(cx, &args.repo, &args.workspace).await?;
                ws.head.unwrap_or(ws.base)
            }
        };
        let authored_at = Timestamp {
            ms: args.authored_at_ms.unwrap_or_else(now_ms),
            offset_min: 0,
        };
        let files = changes.len();
        let req = CreateChangeset {
            repo: args.repo,
            parents: vec![parent],
            changes,
            author: author(cx),
            authored_at,
            message: args.message,
            metadata: Metadata::new(),
            inline_blobs,
            workspace: Some(args.workspace.clone()),
        };
        let created = self
            .api
            .create_changeset(cx, req)
            .await
            .map_err(ToolError::Api)?;
        bounded(json!({
            "id": created.id.to_string(),
            "parent": parent.to_string(),
            "authored_at_ms": authored_at.ms,
            "workspace": args.workspace,
            "files": files,
            "lsn": created.lsn,
            "state": state_name(created.state),
        }))
    }

    async fn sync(&self, cx: &CallContext, args: SyncArgs) -> Result<CallToolResult, ToolError> {
        check_name("repo", &args.repo)?;
        check_name("workspace", &args.workspace)?;
        if let Some(wc) = &self.working_copy {
            let req = WorkingCopySync {
                repo: args.repo.clone(),
                workspace: args.workspace.clone(),
                onto: args.onto.clone(),
            };
            if let Some(value) = wc.sync(cx, req).await? {
                return bounded(value);
            }
        }
        let onto = match &args.onto {
            Some(rev) => Some(self.resolve(cx, &args.repo, rev).await?),
            None => None,
        };
        let synced = self
            .api
            .sync(cx, ws_ref(&args.repo, &args.workspace), onto)
            .await
            .map_err(ToolError::Api)?;
        let mut value = json!({"workspace": workspace_json(&synced.workspace)});
        put_paths(&mut value, "changed_paths", &synced.changed_paths);
        bounded(value)
    }

    async fn land(&self, cx: &CallContext, args: LandArgs) -> Result<CallToolResult, ToolError> {
        check_name("repo", &args.repo)?;
        check_name("workspace", &args.workspace)?;
        if let Some(line) = &args.line {
            check_name("line", line)?;
        }
        if let Some(wc) = &self.working_copy {
            let req = WorkingCopyLand {
                repo: args.repo.clone(),
                workspace: args.workspace.clone(),
                head: args.head.clone(),
                line: args.line.clone(),
                squash: args.squash.unwrap_or(false),
                merge: merge_mode(args.merge),
            };
            if let Some(value) = wc.land(cx, req).await? {
                return bounded(value);
            }
        }
        let ws = self.workspace(cx, &args.repo, &args.workspace).await?;
        let head = match &args.head {
            Some(rev) => self.resolve(cx, &args.repo, rev).await?,
            None => ws
                .head
                .ok_or_else(|| invalid("the workspace has no drafts to land"))?,
        };
        let line = args.line.unwrap_or(ws.line);
        let req = Land {
            repo: args.repo,
            head,
            line: line.clone(),
            squash: args.squash.unwrap_or(false),
            merge: merge_mode(args.merge),
            deadline_ms: None,
        };
        let landed = self.api.land(cx, req).await.map_err(ToolError::Api)?;
        let base = json!({"head": head.to_string(), "line": line});
        let value = match landed {
            Landed::Landed {
                landed,
                line_version,
                pos,
                rebased,
                merged_paths,
            } => {
                let mut value = base;
                value["outcome"] = json!("landed");
                value["landed"] = json!(landed.to_string());
                value["line_version"] = json!(line_version);
                value["pos"] = json!(pos);
                value["rebased"] = json!(rebased);
                put_paths(&mut value, "merged_paths", &merged_paths);
                value
            }
            Landed::Queued { pr_url } => {
                let (url, cut) = clip(&pr_url, URL_MAX_BYTES);
                let mut value = base;
                value["outcome"] = json!("queued");
                value["pr_url"] = json!(url);
                value["pr_url_truncated"] = json!(cut);
                value["detail"] =
                    json!("handed to the PR bridge; the drafts land when the pull request merges");
                value
            }
            _ => {
                let mut value = base;
                value["outcome"] = json!("unknown");
                value
            }
        };
        bounded(value)
    }

    async fn workspace_op(
        &self,
        cx: &CallContext,
        args: WorkspaceArgs,
    ) -> Result<CallToolResult, ToolError> {
        check_name("repo", &args.repo)?;
        if let Some(name) = &args.name {
            check_name("name", name)?;
        }
        let only = |field: &str, present: bool, actions: &str| {
            if present {
                Err(invalid(&format!("`{field}` applies only to {actions}")))
            } else {
                Ok(())
            }
        };
        let open = args.action == WorkspaceAction::Open;
        only("line", args.line.is_some() && !open, "open")?;
        only("at", args.at.is_some() && !open, "open")?;
        only(
            "lease_ms",
            args.lease_ms.is_some()
                && !matches!(args.action, WorkspaceAction::Open | WorkspaceAction::Renew),
            "open and renew",
        )?;
        only(
            "keep_drafts",
            args.keep_drafts.is_some() && args.action != WorkspaceAction::Close,
            "close",
        )?;
        let lease_ms = lease(args.lease_ms)?;
        let named = || {
            args.name
                .clone()
                .ok_or_else(|| invalid("`name` is required for this action"))
        };
        let view = match args.action {
            WorkspaceAction::Open => {
                let line = args
                    .line
                    .clone()
                    .ok_or_else(|| invalid("`line` is required to open a workspace"))?;
                check_name("line", &line)?;
                let at = match &args.at {
                    Some(rev) => Some(self.resolve(cx, &args.repo, rev).await?),
                    None => None,
                };
                let req = OpenWorkspace {
                    repo: args.repo.clone(),
                    name: args.name.clone(),
                    line,
                    at,
                    lease_ms,
                };
                self.api.open_workspace(cx, req).await
            }
            WorkspaceAction::Get => {
                self.api
                    .get_workspace(cx, ws_ref(&args.repo, &named()?))
                    .await
            }
            WorkspaceAction::Renew => {
                self.api
                    .renew_lease(cx, ws_ref(&args.repo, &named()?), lease_ms)
                    .await
            }
            WorkspaceAction::Close => {
                let name = named()?;
                let keep = args.keep_drafts.unwrap_or(true);
                self.api
                    .close_workspace(cx, ws_ref(&args.repo, &name), keep)
                    .await
                    .map_err(ToolError::Api)?;
                let (name, _) = clip(&name, FIELD_MAX_BYTES);
                return bounded(json!({"closed": name, "kept_drafts": keep}));
            }
        }
        .map_err(ToolError::Api)?;
        bounded(json!({"workspace": workspace_json(&view)}))
    }

    /// Reads one workspace.
    async fn workspace(
        &self,
        cx: &CallContext,
        repo: &str,
        name: &str,
    ) -> Result<WorkspaceView, ToolError> {
        self.api
            .get_workspace(cx, ws_ref(repo, name))
            .await
            .map_err(ToolError::Api)
    }
}

#[tool_router(router = write_tool_router, vis = "pub(crate)")]
impl<A: RepoApi + WorkspaceApi + Send + Sync + 'static> McpServer<A> {
    #[tool(
        name = "graft_commit",
        description = "Create a draft on a workspace's stack from inline file contents (at most 256 files, 1 MiB). Contents are stored as given. Returns the draft id; land it with graft_land.",
        annotations(
            title = "Commit",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn graft_commit(
        &self,
        Parameters(args): Parameters<CommitArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.commit(&cx, args).await
        })
        .await)
    }

    #[tool(
        name = "graft_sync",
        description = "Rebase a workspace's stack onto its line tip (or `onto`). Lists the changed paths, at most 200.",
        annotations(
            title = "Sync",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn graft_sync(
        &self,
        Parameters(args): Parameters<SyncArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.sync(&cx, args).await
        })
        .await)
    }

    #[tool(
        name = "graft_land",
        description = "Land a workspace's stack on a line. On a git-primary line it is handed to the PR bridge and returns `queued` with the pull request URL.",
        annotations(
            title = "Land",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn graft_land(
        &self,
        Parameters(args): Parameters<LandArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.land(&cx, args).await
        })
        .await)
    }

    #[tool(
        name = "graft_conflicts",
        description = "What a workspace's stack conflicts on against its line tip (or `onto`): per path the base, ours and theirs ids and kinds, and diff3 markers when all sides are text (clipped to 16 KiB; read larger sides with graft_read by id). A binary path gives ids only, with `markers: null` and `reason: \"binary\"`. Paged by path. Resolve with graft_resolve.",
        annotations(
            title = "Conflicts",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn graft_conflicts(
        &self,
        Parameters(args): Parameters<ConflictsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.conflicts(&cx, args).await
        })
        .await)
    }

    #[tool(
        name = "graft_resolve",
        description = "Rebase a workspace's stack onto `onto` in one step, using your content (or delete) for each conflicting path; other paths line-merge as in graft_sync. Moves the workspace only if its head is `expected_head`. Then land with graft_land.",
        annotations(
            title = "Resolve",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn graft_resolve(
        &self,
        Parameters(args): Parameters<ResolveArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.resolve_stack(&cx, args).await
        })
        .await)
    }

    #[tool(
        name = "graft_revert",
        description = "Create a draft that inverts a changeset, applied on `onto` (default: the workspace head). Land it with graft_land.",
        annotations(
            title = "Revert",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn graft_revert(
        &self,
        Parameters(args): Parameters<RevertArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.revert(&cx, args).await
        })
        .await)
    }

    #[tool(
        name = "graft_workspace",
        description = "Open, read, renew or close a workspace. A workspace holds a stack of drafts on a line, under a lease.",
        annotations(
            title = "Workspace",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn graft_workspace(
        &self,
        Parameters(args): Parameters<WorkspaceArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.workspace_op(&cx, args).await
        })
        .await)
    }
}

/// The merge a land asks for: the argument, a line merge by default.
fn merge_mode(arg: Option<MergeArg>) -> MergeMode {
    match arg.unwrap_or_default() {
        MergeArg::File => MergeMode::File,
        MergeArg::Line => MergeMode::Line,
    }
}

fn invalid(message: &str) -> ToolError {
    ToolError::Invalid(message.to_owned())
}

/// A repository, workspace or line name: present, short, printable. The API
/// applies its own naming rules after this.
fn check_name(field: &str, value: &str) -> Result<(), ToolError> {
    if value.is_empty() {
        return Err(invalid(&format!("`{field}` is empty")));
    }
    if value.len() > FIELD_MAX_BYTES {
        return Err(invalid(&format!(
            "`{field}` is longer than {FIELD_MAX_BYTES} bytes"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(invalid(&format!("`{field}` contains a control character")));
    }
    Ok(())
}

fn lease(requested: Option<u64>) -> Result<u64, ToolError> {
    match requested.unwrap_or(LEASE_DEFAULT_MS) {
        ms if (LEASE_MIN_MS..=LEASE_MAX_MS).contains(&ms) => Ok(ms),
        ms => Err(invalid(&format!(
            "lease_ms {ms} is outside {LEASE_MIN_MS} to {LEASE_MAX_MS}"
        ))),
    }
}

/// A staged path: the path rules, and never inside `.graft`.
///
/// Windows drops trailing dots and spaces from a name, so `.graft.` and
/// `.graft ` name the reserved directory there. They are refused here too.
fn staged_path(s: &str) -> Result<RepoPath, ToolError> {
    let path = parse_path(s)?;
    if path.components().any(|c| {
        c.trim_end_matches(['.', ' '])
            .eq_ignore_ascii_case(RESERVED_DIR)
    }) {
        return Err(ToolError::Api(ApiError::InvalidPath {
            path: clip(s, FIELD_MAX_BYTES).0.to_owned(),
            reason: "reserved".into(),
        }));
    }
    Ok(path)
}

/// Turns inline files into sorted changes and the blob frames they need.
/// Sizes are checked before anything is decoded.
pub(crate) fn stage(files: &[FileChange]) -> Result<(Vec<Change>, Vec<Frame>), ToolError> {
    if files.len() > COMMIT_MAX_FILES {
        return Err(ToolError::Api(ApiError::LimitExceeded {
            limit: "commit-files".into(),
            max: COMMIT_MAX_FILES as u64,
        }));
    }
    let mut total: u64 = 0;
    let mut paths = BTreeSet::new();
    let mut blobs = BTreeSet::new();
    let mut changes = Vec::with_capacity(files.len());
    let mut frames = Vec::new();
    for file in files {
        let path = staged_path(&file.path)?;
        if !paths.insert(path.clone()) {
            return Err(invalid(&format!(
                "path `{}` is named twice",
                clip(&file.path, FIELD_MAX_BYTES).0
            )));
        }
        match file.op.unwrap_or_default() {
            FileOp::Delete => {
                if file.content.is_some() || file.encoding.is_some() || file.kind.is_some() {
                    return Err(invalid(&format!(
                        "delete of `{}` takes no content, encoding or kind",
                        clip(&file.path, FIELD_MAX_BYTES).0
                    )));
                }
                changes.push(Change::Delete { path });
            }
            FileOp::Put => {
                let content = file.content.as_deref().ok_or_else(|| {
                    invalid(&format!(
                        "put of `{}` needs content",
                        clip(&file.path, FIELD_MAX_BYTES).0
                    ))
                })?;
                let encoding = file.encoding.unwrap_or_default();
                let size = match encoding {
                    Encoding::Utf8 => content.len(),
                    Encoding::Hex => content.len() / 2,
                };
                total = total.saturating_add(size as u64);
                if total > COMMIT_MAX_BYTES {
                    return Err(ToolError::Api(ApiError::LimitExceeded {
                        limit: "commit-bytes".into(),
                        max: COMMIT_MAX_BYTES,
                    }));
                }
                let data = match encoding {
                    Encoding::Utf8 => content.as_bytes().to_vec(),
                    Encoding::Hex => unhex(content).ok_or_else(|| {
                        invalid(&format!(
                            "content of `{}` is not hex",
                            clip(&file.path, FIELD_MAX_BYTES).0
                        ))
                    })?,
                };
                let kind = match file.kind.unwrap_or_default() {
                    FileKind::File => EntryKind::File,
                    FileKind::Exec => EntryKind::Exec,
                    FileKind::Symlink => EntryKind::Symlink,
                };
                let id = BlobId::hash(&data).0;
                if blobs.insert(id) {
                    frames.push(Frame::encode(ObjectKind::Blob, &data));
                }
                changes.push(Change::Put { path, kind, id });
            }
        }
    }
    changes.sort_by(|a, b| a.path().cmp(b.path()));
    Ok((changes, frames))
}

/// Turns `graft_resolve` entries into resolutions, under `graft_commit`'s
/// limits. Sizes are checked before anything is decoded.
fn resolutions(files: &[FileChange]) -> Result<Vec<PathResolution>, ToolError> {
    // `stage` applies every path, size and shape rule; its blob ids are not
    // needed, only the decoded bytes below.
    stage(files)?;
    files
        .iter()
        .map(|file| {
            let path = staged_path(&file.path)?;
            let resolution = match file.op.unwrap_or_default() {
                FileOp::Delete => Resolution::Delete,
                FileOp::Put => {
                    let content = file.content.as_deref().unwrap_or_default();
                    let data = match file.encoding.unwrap_or_default() {
                        Encoding::Utf8 => content.as_bytes().to_vec(),
                        // `stage` above refused bad hex, so this decodes.
                        Encoding::Hex => unhex(content).unwrap_or_default(),
                    };
                    Resolution::Put {
                        kind: match file.kind.unwrap_or_default() {
                            FileKind::File => EntryKind::File,
                            FileKind::Exec => EntryKind::Exec,
                            FileKind::Symlink => EntryKind::Symlink,
                        },
                        content: ResolvedContent::Inline(data),
                    }
                }
            };
            Ok(PathResolution { path, resolution })
        })
        .collect()
}

fn side_json(side: Option<&ConflictSide>) -> Value {
    side.map_or(
        Value::Null,
        |s| json!({"id": s.id.to_hex(), "kind": kind_name(s.kind)}),
    )
}

/// One conflicting path: ids, kinds, and markers clipped to
/// [`CONFLICT_MARKERS_MAX_BYTES`].
fn conflict_json(c: &ConflictView) -> Value {
    let markers = c.markers.as_deref().map(String::from_utf8_lossy);
    let (text, cut) = markers.as_deref().map_or((None, false), |m| {
        let (t, cut) = clip(m, CONFLICT_MARKERS_MAX_BYTES);
        (Some(t), cut)
    });
    json!({
        "path": c.path.as_str(),
        "base": side_json(c.base.as_ref()),
        "ours": side_json(c.ours.as_ref()),
        "theirs": side_json(c.theirs.as_ref()),
        "text_mergeable": c.text_mergeable,
        "markers": text,
        "markers_truncated": cut,
        "reason": c.reason.map(|r| match r {
            ConflictReason::Binary => "binary",
            ConflictReason::TooLarge => "too-large",
            ConflictReason::Kind => "kind",
            ConflictReason::Clash => "clash",
            _ => "unknown",
        }),
    })
}

/// Decodes exactly two ASCII hex digits per byte. `from_str_radix` would also
/// accept a sign, so each digit is checked on its own.
fn unhex(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    bytes
        .chunks(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some((hi * 16 + lo) as u8)
        })
        .collect()
}

/// The author of a commit: the caller, as its token names it.
fn author(cx: &CallContext) -> Author {
    let principal = &cx.principal;
    Author {
        name: principal.subject.clone(),
        email: String::new(),
        kind: match principal.kind {
            PrincipalKind::Human => AuthorKind::Human,
            PrincipalKind::Agent => AuthorKind::Agent,
            PrincipalKind::Ci => AuthorKind::Ci,
            PrincipalKind::Service => AuthorKind::Bot,
        },
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn ws_ref(repo: &str, name: &str) -> WorkspaceRef {
    WorkspaceRef {
        repo: repo.to_owned(),
        name: name.to_owned(),
    }
}

fn workspace_json(ws: &WorkspaceView) -> Value {
    let text = |s: &str| clip(s, FIELD_MAX_BYTES).0.to_owned();
    json!({
        "name": text(&ws.name),
        "owner": text(&ws.owner),
        "session": ws.session.as_deref().map(text),
        "line": text(&ws.line),
        "base": ws.base.to_string(),
        "head": ws.head.map(|h: ChangesetId| h.to_string()),
        "lease_until_ms": ws.lease_until_ms,
    })
}

/// Puts at most [`PATHS_MAX`] paths under `key`, with their total count and
/// whether the list was cut.
fn put_paths(value: &mut Value, key: &str, paths: &[RepoPath]) {
    value[key] = json!(
        paths
            .iter()
            .take(PATHS_MAX)
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
    );
    value[format!("{key}_total")] = json!(paths.len());
    value[format!("{key}_truncated")] = json!(paths.len() > PATHS_MAX);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(path: &str, content: &str) -> FileChange {
        FileChange {
            path: path.into(),
            op: None,
            content: Some(content.into()),
            encoding: None,
            kind: None,
        }
    }

    fn err_kind(e: ToolError) -> Value {
        e.into_result().structured_content.unwrap()["kind"].clone()
    }

    #[test]
    fn staging_sorts_and_dedupes_blobs() {
        let (changes, frames) = stage(&[put("b", "same"), put("a", "same")]).unwrap();
        let paths: Vec<&str> = changes.iter().map(|c| c.path().as_str()).collect();
        assert_eq!(paths, ["a", "b"]);
        assert_eq!(frames.len(), 1, "one blob for two identical files");
    }

    #[test]
    fn staging_refuses_traversal_and_the_graft_dir() {
        for bad in [
            "../x",
            "a/../b",
            "./a",
            "/abs",
            "a//b",
            ".graft/config",
            "src/.GRAFT/x",
            ".graft./config",
            ".graft /config",
            ".GRAFT./x",
            "src/.graft . /x",
        ] {
            let e = stage(&[put(bad, "x")]).expect_err(bad);
            let kind = err_kind(e);
            assert!(
                kind == "invalid_argument" || kind == "invalid_path",
                "{bad}: {kind}"
            );
        }
        assert!(stage(&[put(".graftignore", "x")]).is_ok());
    }

    #[test]
    fn staging_refuses_oversized_and_duplicate_input() {
        let big = "x".repeat(COMMIT_MAX_BYTES as usize + 1);
        assert_eq!(
            err_kind(stage(&[put("a", &big)]).unwrap_err()),
            "limit_exceeded"
        );
        let many: Vec<FileChange> = (0..=COMMIT_MAX_FILES)
            .map(|i| put(&format!("f{i}"), ""))
            .collect();
        assert_eq!(err_kind(stage(&many).unwrap_err()), "limit_exceeded");
        assert_eq!(
            err_kind(stage(&[put("a", "1"), put("a", "2")]).unwrap_err()),
            "invalid_argument"
        );
    }

    #[test]
    fn hex_content_decodes_and_bad_hex_is_refused() {
        let mut file = put("bin", "00ff7F");
        file.encoding = Some(Encoding::Hex);
        let (changes, frames) = stage(&[file]).unwrap();
        assert_eq!(frames[0].body, [0x00, 0xff, 0x7f]);
        assert!(matches!(changes[0], Change::Put { .. }));
        let mut odd = put("bin", "abc");
        odd.encoding = Some(Encoding::Hex);
        assert!(stage(&[odd]).is_err());
    }

    #[test]
    fn hex_refuses_signs_and_non_hex_digits() {
        for bad in ["+f", "f+", "-1", "0x", "g0", "é0"] {
            let mut file = put("bin", bad);
            file.encoding = Some(Encoding::Hex);
            assert!(stage(&[file]).is_err(), "{bad} must not decode");
        }
    }

    #[test]
    fn a_delete_carries_no_content() {
        let mut file = put("a", "x");
        file.op = Some(FileOp::Delete);
        assert!(stage(&[file]).is_err());
    }
}
