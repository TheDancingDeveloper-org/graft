//! `graft_draft_overlap` (WI-2310, decision D24): which other live
//! workspaces have open drafts on these paths, before anyone lands.
//! Read-only, and gated by the `repo:<r>:drafts` scope on the server.
//!
//! The answer names workspaces, owners, Vogt sessions and work items, draft
//! age and the shared paths. It never carries contents or head ids.

use std::collections::BTreeMap;

use graft_proto::{
    api::{RepoApi, WorkspaceApi},
    context::CallContext,
    types::{DraftOverlap, MAX_OVERLAP_PAGE, MAX_OVERLAP_PATHS, OverlappingDrafts, Page},
};
use rmcp::{
    ErrorData, RoleServer, handler::server::wrapper::Parameters, model::CallToolResult,
    service::RequestContext, tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::render::{ToolError, bounded, fits};
use crate::server::{McpServer, parse_path, respond};

/// Hits per page by default.
const OVERLAP_DEFAULT: u32 = 50;
/// The line asked about when none is named.
const DEFAULT_LINE: &str = "main";

/// Arguments of `graft_draft_overlap`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DraftOverlapArgs {
    /// Repository name.
    pub repo: String,
    /// The line (default `main`).
    pub line: Option<String>,
    /// Files or directory prefixes you are about to change (1 to 64).
    pub paths: Vec<String>,
    /// Include your own workspaces (default false).
    pub include_own: Option<bool>,
    /// Hits per page (1 to 200, default 50).
    pub limit: Option<u32>,
    /// Resume token, from the previous result's `next`.
    pub cursor: Option<String>,
}

/// One overlap page grouped by workspace, in first-hit order.
pub(crate) fn overlap_json(line: &str, o: &DraftOverlap) -> Value {
    let mut paths: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for h in &o.hits {
        paths
            .entry(h.workspace.as_str())
            .or_default()
            .push(h.path.as_str());
    }
    let overlaps: Vec<Value> = o
        .workspaces
        .iter()
        .map(|w| {
            json!({
                "workspace": w.workspace,
                "owner": w.owner,
                "session": w.session,
                "work_item": w.work_item,
                "draft_age_ms": w.draft_age_ms,
                "lease_until_ms": w.lease_until_ms,
                "paths": paths.get(w.workspace.as_str()).cloned().unwrap_or_default(),
            })
        })
        .collect();
    json!({ "line": line, "overlaps": overlaps, "next": o.next })
}

impl<A: RepoApi + WorkspaceApi + Send + Sync + 'static> McpServer<A> {
    async fn draft_overlap(
        &self,
        cx: &CallContext,
        args: DraftOverlapArgs,
    ) -> Result<CallToolResult, ToolError> {
        if args.paths.is_empty() || args.paths.len() > MAX_OVERLAP_PATHS {
            return Err(ToolError::Invalid(format!(
                "paths must name 1 to {MAX_OVERLAP_PATHS} files or directories"
            )));
        }
        let paths = args
            .paths
            .iter()
            .map(|p| parse_path(p))
            .collect::<Result<Vec<_>, _>>()?;
        let mut limit = match args.limit.unwrap_or(OVERLAP_DEFAULT) {
            n if (1..=MAX_OVERLAP_PAGE).contains(&n) => n,
            n => {
                return Err(ToolError::Invalid(format!(
                    "limit {n} is outside 1 to {MAX_OVERLAP_PAGE}"
                )));
            }
        };
        let line = args.line.unwrap_or_else(|| DEFAULT_LINE.to_owned());
        // Halve the page until the result fits the cap.
        loop {
            let req = OverlappingDrafts {
                repo: args.repo.clone(),
                line: line.clone(),
                paths: paths.clone(),
                include_own: args.include_own.unwrap_or(false),
                page: Page {
                    limit,
                    token: args.cursor.clone(),
                },
            };
            let page = self
                .api
                .overlapping_drafts(cx, req)
                .await
                .map_err(ToolError::Api)?;
            let value = overlap_json(&line, &page);
            if fits(&CallToolResult::structured(value.clone())) || limit == 1 {
                return bounded(value);
            }
            limit /= 2;
        }
    }
}

#[tool_router(router = drafts_tool_router, vis = "pub(crate)")]
impl<A: RepoApi + WorkspaceApi + Send + Sync + 'static> McpServer<A> {
    #[tool(
        name = "graft_draft_overlap",
        description = "Before you start on a file: which other agents' live workspaces have open drafts changing these paths (files or directory prefixes), with owner, Vogt session or work item, draft age and the shared paths. Never contents. Needs the repo:<r>:drafts scope. Keep paging while `next` is set; a page can be short.",
        annotations(
            title = "Draft overlap",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn graft_draft_overlap(
        &self,
        Parameters(args): Parameters<DraftOverlapArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.draft_overlap(&cx, args).await
        })
        .await)
    }
}
