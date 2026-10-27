//! Phase-2 review and attestation MCP tools. Every read is paginated and
//! shrinks its requested page until the complete MCP result fits the cap.

use graft_model::{AuthorKind, ChangesetId, RepoPath};
use graft_proto::{
    api::{RepoApi, WorkspaceApi},
    types::{
        AttestKind, AttestStatus, AttestV2, Attestation, AttestationPage, AttestationSelector,
        GetAttestations, GetLandChecks, LandCheck, LandCheckPage, LandCheckState, MAX_REVIEW_PAGE,
        Page, ReviewItem, ReviewQueue, ReviewQueuePage,
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
use crate::server::{McpServer, respond};

const REVIEW_DEFAULT: u32 = 50;

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// Kind of attestation to record or select.
pub enum AttestKindArg {
    /// Automated check.
    Check,
    /// Reviewer verdict.
    Review,
    /// Approval verdict.
    Approval,
}

impl From<AttestKindArg> for AttestKind {
    fn from(value: AttestKindArg) -> Self {
        match value {
            AttestKindArg::Check => Self::Check,
            AttestKindArg::Review => Self::Review,
            AttestKindArg::Approval => Self::Approval,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// Outcome of an attestation.
pub enum AttestStatusArg {
    /// Passed or approved.
    Pass,
    /// Failed or changes requested.
    Fail,
    /// Started without a verdict.
    Pending,
    /// The attester failed.
    Error,
}

impl From<AttestStatusArg> for AttestStatus {
    fn from(value: AttestStatusArg) -> Self {
        match value {
            AttestStatusArg::Pass => Self::Pass,
            AttestStatusArg::Fail => Self::Fail,
            AttestStatusArg::Pending => Self::Pending,
            AttestStatusArg::Error => Self::Error,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Arguments of `graft_review_queue`.
pub struct ReviewQueueArgs {
    /// Repository name.
    pub repo: String,
    /// Line whose landed changesets await review.
    pub line: String,
    /// Only landings journalled after this LSN (default 0).
    pub since_lsn: Option<u64>,
    /// Only landings touching this file or directory prefix.
    pub path_prefix: Option<String>,
    /// Only landings changing at most this many lines.
    pub max_lines: Option<u32>,
    /// Items per page (1 to 1000, default 50).
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Arguments of `graft_get_attestations`.
pub struct GetAttestationsArgs {
    /// Repository name.
    pub repo: String,
    /// One changeset to read. Give this or all line-range fields.
    pub changeset: Option<String>,
    /// Line for a position range. Give with `from_pos` and `to_pos`.
    pub line: Option<String>,
    /// First line position, inclusive.
    pub from_pos: Option<u64>,
    /// Last line position, inclusive.
    pub to_pos: Option<u64>,
    /// Only these kinds; empty or omitted means all.
    #[serde(default)]
    pub kinds: Vec<AttestKindArg>,
    /// Items per page (1 to 1000, default 50).
    pub limit: Option<u32>,
    /// Resume token from the previous result's `next`.
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Arguments of `graft_get_land_checks`.
pub struct GetLandChecksArgs {
    /// Repository name.
    pub repo: String,
    /// Line whose landed check states to read.
    pub line: String,
    /// First line position wanted (default 0).
    pub from_pos: Option<u64>,
    /// Items per page (1 to 1000, default 50).
    pub limit: Option<u32>,
    /// Resume token from the previous result's `next`.
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Arguments of `graft_attest`.
pub struct AttestArgs {
    /// Repository name.
    pub repo: String,
    /// Changeset id to attest.
    pub changeset: String,
    /// Attestation kind.
    pub kind: AttestKindArg,
    /// Check or review name.
    pub check: String,
    /// Attestation outcome.
    pub status: AttestStatusArg,
    /// Display name of the attester; identity still comes from authentication.
    pub by: Option<String>,
    /// Link to the check or review, or an empty string.
    pub url: Option<String>,
}

fn page_limit(limit: Option<u32>) -> Result<u32, ToolError> {
    match limit.unwrap_or(REVIEW_DEFAULT) {
        0 => Err(ToolError::Invalid("limit must be at least 1".into())),
        n => Ok(n.min(MAX_REVIEW_PAGE)),
    }
}

fn changeset(value: &str) -> Result<ChangesetId, ToolError> {
    value.parse().map_err(|_| {
        ToolError::Invalid("changeset must be `c` followed by 64 lowercase hex characters".into())
    })
}

fn kind_name(kind: AttestKind) -> &'static str {
    match kind {
        AttestKind::Check => "check",
        AttestKind::Review => "review",
        AttestKind::Approval => "approval",
        _ => "unknown",
    }
}

fn status_name(status: AttestStatus) -> &'static str {
    match status {
        AttestStatus::Pass => "pass",
        AttestStatus::Fail => "fail",
        AttestStatus::Pending => "pending",
        AttestStatus::Error => "error",
        _ => "unknown",
    }
}

fn state_name(state: LandCheckState) -> &'static str {
    match state {
        LandCheckState::Pending => "pending",
        LandCheckState::Good => "good",
        LandCheckState::Bad => "bad",
        LandCheckState::Reverted => "reverted",
        _ => "unknown",
    }
}

fn author_kind_name(kind: AuthorKind) -> &'static str {
    match kind {
        AuthorKind::Human => "human",
        AuthorKind::Agent => "agent",
        AuthorKind::Bot => "bot",
        AuthorKind::Ci => "ci",
    }
}

fn review_item(item: ReviewItem) -> Value {
    json!({
        "changeset": item.changeset.to_string(),
        "pos": item.pos,
        "lsn": item.lsn,
        "author": {
            "name": clip(&item.author.name, FIELD_MAX_BYTES).0,
            "email": clip(&item.author.email, FIELD_MAX_BYTES).0,
            "kind": author_kind_name(item.author.kind),
        },
        "paths_touched": item.paths_touched,
        "lines_changed": item.lines_changed,
    })
}

fn attestation(item: Attestation) -> Value {
    json!({
        "changeset": item.changeset.to_string(),
        "kind": kind_name(item.kind),
        "check": clip(&item.check, FIELD_MAX_BYTES).0,
        "status": status_name(item.status),
        "by": item.by.as_deref().map(|v| clip(v, FIELD_MAX_BYTES).0),
        "url": clip(&item.url, FIELD_MAX_BYTES).0,
    })
}

fn land_check(item: LandCheck) -> Value {
    json!({
        "pos": item.pos,
        "changeset": item.changeset.to_string(),
        "state": state_name(item.state),
        "revert": item.revert.map(|id| id.to_string()),
    })
}

impl<A: RepoApi + WorkspaceApi + Send + Sync + 'static> McpServer<A> {
    async fn review_queue_result(
        &self,
        cx: &graft_proto::CallContext,
        args: ReviewQueueArgs,
    ) -> Result<CallToolResult, ToolError> {
        let limit = page_limit(args.limit)?;
        let path_prefix = args
            .path_prefix
            .as_deref()
            .map(|path| RepoPath::new(path).map_err(|e| ToolError::Invalid(e.to_string())))
            .transpose()?;
        fit(
            limit,
            |size| {
                self.api.review_queue(
                    cx,
                    ReviewQueue {
                        repo: args.repo.clone(),
                        line: args.line.clone(),
                        since_lsn: args.since_lsn.unwrap_or(0),
                        path_prefix: path_prefix.clone(),
                        max_lines: args.max_lines,
                        page: Page {
                            limit: size,
                            token: None,
                        },
                    },
                )
            },
            |page: ReviewQueuePage, _| {
                json!({
                    "items": page.items.into_iter().map(review_item).collect::<Vec<_>>(),
                    "next_lsn": page.next_lsn,
                })
            },
        )
        .await
    }

    async fn attestations_result(
        &self,
        cx: &graft_proto::CallContext,
        args: GetAttestationsArgs,
    ) -> Result<CallToolResult, ToolError> {
        let selector = match (args.changeset.as_deref(), args.line, args.from_pos, args.to_pos) {
            (Some(id), None, None, None) => AttestationSelector::ByChangeset(changeset(id)?),
            (None, Some(line), Some(from_pos), Some(to_pos)) if from_pos <= to_pos => {
                AttestationSelector::ByLineRange { line, from_pos, to_pos }
            }
            _ => return Err(ToolError::Invalid(
                "give `changeset`, or give `line`, `from_pos`, and `to_pos` with from_pos <= to_pos".into(),
            )),
        };
        let limit = page_limit(args.limit)?;
        let kinds = args.kinds.into_iter().map(Into::into).collect::<Vec<_>>();
        fit(
            limit,
            |size| self.api.get_attestations(
                cx,
                GetAttestations {
                    repo: args.repo.clone(),
                    selector: selector.clone(),
                    kinds: kinds.clone(),
                    page: Page { limit: size, token: args.cursor.clone() },
                },
            ),
            |page: AttestationPage, _| json!({
                "attestations": page.attestations.into_iter().map(attestation).collect::<Vec<_>>(),
                "next": page.next,
            }),
        )
        .await
    }

    async fn land_checks_result(
        &self,
        cx: &graft_proto::CallContext,
        args: GetLandChecksArgs,
    ) -> Result<CallToolResult, ToolError> {
        let limit = page_limit(args.limit)?;
        fit(
            limit,
            |size| {
                self.api.get_land_checks(
                    cx,
                    GetLandChecks {
                        repo: args.repo.clone(),
                        line: args.line.clone(),
                        from_pos: args.from_pos.unwrap_or(0),
                        page: Page {
                            limit: size,
                            token: args.cursor.clone(),
                        },
                    },
                )
            },
            |page: LandCheckPage, _| {
                json!({
                    "rows": page.rows.into_iter().map(land_check).collect::<Vec<_>>(),
                    "next": page.next,
                })
            },
        )
        .await
    }
}

#[tool_router(router = review_tool_router, vis = "pub(crate)")]
impl<A: RepoApi + WorkspaceApi + Send + Sync + 'static> McpServer<A> {
    #[tool(
        name = "graft_review_queue",
        description = "List landed changesets that still need a review verdict. Pages of up to 1000 items; continue from `next_lsn`.",
        annotations(
            title = "Review queue",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn graft_review_queue(
        &self,
        Parameters(args): Parameters<ReviewQueueArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.review_queue_result(&cx, args).await
        })
        .await)
    }

    #[tool(
        name = "graft_get_attestations",
        description = "Read a changeset's or line range's attestations. Pages of up to 1000 items; follow `next`.",
        annotations(
            title = "Get attestations",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn graft_get_attestations(
        &self,
        Parameters(args): Parameters<GetAttestationsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.attestations_result(&cx, args).await
        })
        .await)
    }

    #[tool(
        name = "graft_get_land_checks",
        description = "Read the bounded check state of landed changesets on a line. Pages of up to 1000 items; follow `next`.",
        annotations(
            title = "Get land checks",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn graft_get_land_checks(
        &self,
        Parameters(args): Parameters<GetLandChecksArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.land_checks_result(&cx, args).await
        })
        .await)
    }

    #[tool(
        name = "graft_attest",
        description = "Post one check, review, or approval outcome for a changeset. This records a verdict; it does not land or gate anything.",
        annotations(
            title = "Attest",
            read_only_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn graft_attest(
        &self,
        Parameters(args): Parameters<AttestArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            let id = changeset(&args.changeset)?;
            self.api
                .attest_v2(
                    &cx,
                    AttestV2 {
                        repo: args.repo,
                        changeset: id,
                        kind: args.kind.into(),
                        check: args.check,
                        status: args.status.into(),
                        by: args.by,
                        url: args.url.unwrap_or_default(),
                    },
                )
                .await
                .map_err(ToolError::Api)?;
            bounded(json!({"changeset": id.to_string(), "recorded": true}))
        })
        .await)
    }
}
