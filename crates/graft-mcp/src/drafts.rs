//! `graft_draft_overlap` (WI-2310, decision D24): which other live
//! workspaces have open drafts on these paths, before anyone lands.
//! Read-only, and gated by the `repo:<r>:drafts` scope on the server.
//!
//! The answer names workspaces, owners, sessions and work items, draft
//! age and the shared paths. It never carries contents or head ids.
//!
//! Session and work item come from the workspace view when the server has
//! already resolved them, else from the draft's metadata through
//! [`graft_proto::attribution::get`]: `agent.*` wins over the legacy keys.

use std::collections::BTreeMap;

use graft_model::changeset::Metadata;
use graft_proto::{
    api::{RepoApi, WorkspaceApi},
    attribution::{self, Key},
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
///
/// `meta` is the head-draft metadata of each workspace, keyed by workspace
/// name. Session and work item already set on the view win; otherwise they
/// are read with [`attribution_fields`].
pub(crate) fn overlap_json(line: &str, o: &DraftOverlap) -> Value {
    overlap_json_with(line, o, &BTreeMap::new())
}

pub(crate) fn overlap_json_with(
    line: &str,
    o: &DraftOverlap,
    meta: &BTreeMap<&str, &Metadata>,
) -> Value {
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
            let empty = Metadata::new();
            let ws_meta = meta.get(w.workspace.as_str()).copied().unwrap_or(&empty);
            let (session, work_item) =
                attribution_fields(w.session.as_deref(), w.work_item.as_deref(), ws_meta);
            json!({
                "workspace": w.workspace,
                "owner": w.owner,
                "session": session,
                "work_item": work_item,
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

/// Session and work item for one overlapping workspace.
///
/// A field already set on the workspace view is used as-is (the server
/// resolved it). An absent field is read from `meta` with
/// [`attribution::get`], so `agent.*` wins and a legacy key still shows.
fn attribution_fields(
    session: Option<&str>,
    work_item: Option<&str>,
    meta: &Metadata,
) -> (Option<String>, Option<String>) {
    let pick = |given: Option<&str>, key: Key| {
        given
            .map(str::to_owned)
            .or_else(|| attribution::get(meta, key).map(str::to_owned))
    };
    (pick(session, Key::Session), pick(work_item, Key::WorkItem))
}

#[tool_router(router = drafts_tool_router, vis = "pub(crate)")]
impl<A: RepoApi + WorkspaceApi + Send + Sync + 'static> McpServer<A> {
    #[tool(
        name = "graft_draft_overlap",
        description = "Before you start on a file: which other agents' live workspaces have open drafts changing these paths (files or directory prefixes), with owner, session or work item, draft age and the shared paths. Never contents. Needs the repo:<r>:drafts scope. Keep paging while `next` is set; a page can be short.",
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

#[cfg(test)]
mod tests {
    use super::*;
    use graft_model::RepoPath;
    use graft_proto::attribution::{self, Key};
    use graft_proto::types::{DraftHit, DraftWorkspace};

    fn meta(pairs: &[(&str, &str)]) -> Metadata {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn attribution_new_only_legacy_only_and_both() {
        let session = Key::Session;
        let item = Key::WorkItem;
        let model = Key::Model;
        let co = Key::Coauthors;

        let new_only = meta(&[
            (session.name(), "ses_new"),
            (item.name(), "WI-1"),
            (model.name(), "m-new"),
            (co.name(), "a\nb"),
        ]);
        assert_eq!(
            attribution_fields(None, None, &new_only),
            (Some("ses_new".into()), Some("WI-1".into()))
        );
        assert_eq!(attribution::get(&new_only, model), Some("m-new"));
        assert_eq!(attribution::get(&new_only, co), Some("a\nb"));

        let legacy = meta(&[
            (session.legacy(), "ses_old"),
            (item.legacy(), "WI-0"),
            (model.legacy(), "m-old"),
            (co.legacy(), "c"),
        ]);
        assert_eq!(
            attribution_fields(None, None, &legacy),
            (Some("ses_old".into()), Some("WI-0".into()))
        );
        assert_eq!(attribution::get(&legacy, model), Some("m-old"));
        assert_eq!(attribution::get(&legacy, co), Some("c"));

        let both = meta(&[
            (session.legacy(), "ses_old"),
            (session.name(), "ses_new"),
            (item.legacy(), "WI-0"),
            (item.name(), "WI-1"),
        ]);
        assert_eq!(
            attribution_fields(None, None, &both),
            (Some("ses_new".into()), Some("WI-1".into()))
        );
        // A field the server already resolved is not replaced by metadata.
        assert_eq!(
            attribution_fields(Some("ses_view"), Some("WI-view"), &both),
            (Some("ses_view".into()), Some("WI-view".into()))
        );
    }

    #[test]
    fn overlap_output_is_the_same_for_legacy_and_agent_keys() {
        let page = DraftOverlap {
            hits: vec![DraftHit {
                path: RepoPath::new("src/lib.rs").unwrap(),
                workspace: "ws".into(),
            }],
            workspaces: vec![DraftWorkspace {
                workspace: "ws".into(),
                owner: "ada".into(),
                session: None,
                work_item: None,
                draft_age_ms: 10,
                lease_until_ms: 20,
            }],
            next: None,
        };
        let legacy = meta(&[
            (Key::Session.legacy(), "ses"),
            (Key::WorkItem.legacy(), "WI-1"),
            (Key::Model.legacy(), "m"),
            (Key::Coauthors.legacy(), "c"),
        ]);
        let agent = meta(&[
            (Key::Session.name(), "ses"),
            (Key::WorkItem.name(), "WI-1"),
            (Key::Model.name(), "m"),
            (Key::Coauthors.name(), "c"),
        ]);
        let from_legacy = overlap_json_with("main", &page, &BTreeMap::from([("ws", &legacy)]));
        let from_agent = overlap_json_with("main", &page, &BTreeMap::from([("ws", &agent)]));
        assert_eq!(from_legacy, from_agent);
        assert_eq!(from_agent["overlaps"][0]["session"], "ses");
        assert_eq!(from_agent["overlaps"][0]["work_item"], "WI-1");
        // A view that already carries the fields is unchanged by either key set.
        let filled = DraftOverlap {
            workspaces: vec![DraftWorkspace {
                session: Some("ses".into()),
                work_item: Some("WI-1".into()),
                ..page.workspaces[0].clone()
            }],
            ..page.clone()
        };
        assert_eq!(
            overlap_json("main", &filled)["overlaps"][0]["session"],
            "ses"
        );
        assert_eq!(
            overlap_json("main", &filled)["overlaps"][0]["work_item"],
            "WI-1"
        );
    }
}
