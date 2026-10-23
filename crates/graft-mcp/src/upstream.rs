//! `graft_upstream` (WI-2309): what the line did since a workspace's base,
//! and whether its work would merge onto the tip. Read-only.
//!
//! Inside a working copy (stdio) the working copy answers for its own
//! workspace, uncommitted edits included. Otherwise the server answers from
//! the workspace's drafts: the paths the stack changed (`Diff` base..head)
//! are checked against the line with `Diff` base..tip restricted to them,
//! an id-pruned tree diff that never walks history. Each path both sides
//! changed is predicted with the decision D16 line merge on the base, head
//! and tip contents, and the engine's sync planner (`GetConflicts`, run in
//! memory) has the last word on which paths conflict.

use std::collections::BTreeSet;

use graft_model::merge::{MAX_TEXT_BYTES, Merge3, is_text, merge3};
use graft_model::{Change, ChangesetId, EntryKind, RepoPath};
use graft_proto::{
    ApiError,
    api::{RepoApi, WorkspaceApi},
    context::CallContext,
    types::{Diff, GetConflicts, Log, Page, PathAt, ResolveRef, WorkspaceRef},
};
use rmcp::{
    ErrorData, RoleServer, handler::server::wrapper::Parameters, model::CallToolResult,
    service::RequestContext, tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::render::{ToolError, bounded, fits};
use crate::server::{McpServer, respond};
use crate::workcopy::WorkingCopyUpstream;

/// Bound on each list: default and maximum.
const UPSTREAM_DEFAULT: u32 = 50;
const UPSTREAM_MAX: u32 = 100;
/// Paths per `Diff` request (the server's default `max_diff_paths`).
const DIFF_PATHS: usize = 64;
/// The most of the stack's paths checked against the line.
const MAX_OWN_PATHS: usize = 1024;

/// Arguments of `graft_upstream`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpstreamArgs {
    /// Repository name.
    pub repo: String,
    /// The workspace. Inside a working copy it defaults to the working
    /// copy's own; elsewhere it is required.
    pub workspace: Option<String>,
    /// Bound on each list (1 to 100, default 50).
    pub limit: Option<u32>,
    /// Resume token for `changed`, from the previous result's `next`.
    pub cursor: Option<String>,
}

type Side = Option<(EntryKind, Vec<u8>)>;

/// The D16 prediction for a path both sides changed, as JSON fields:
/// `outcome`, `hunks` (1-based inclusive line ranges in ours) and `reason`.
fn predict(
    base: &Side,
    ours: &Side,
    theirs: &Side,
) -> (bool, Vec<[usize; 2]>, Option<&'static str>) {
    if ours == theirs {
        return (true, Vec::new(), None);
    }
    let (Some(b), Some(o), Some(t)) = (base, ours, theirs) else {
        return (false, Vec::new(), Some("clash"));
    };
    let file = |k: EntryKind| matches!(k, EntryKind::File | EntryKind::Exec);
    if !(file(b.0) && file(o.0) && file(t.0)) {
        return (false, Vec::new(), Some("kind"));
    }
    if [&b.1, &o.1, &t.1].iter().any(|d| d.len() >= MAX_TEXT_BYTES) {
        return (false, Vec::new(), Some("too_large"));
    }
    if ![&b.1, &o.1, &t.1].iter().all(|d| is_text(d)) {
        return (false, Vec::new(), Some("binary"));
    }
    match merge3(&b.1, &o.1, &t.1) {
        Merge3::Clean(_) => (true, Vec::new(), None),
        Merge3::Conflicts(regions) => (
            false,
            regions
                .iter()
                .map(|r| [r.start + 1, r.end.max(r.start + 1)])
                .collect(),
            None,
        ),
        Merge3::NotText => (false, Vec::new(), Some("binary")),
        Merge3::TooComplex => (false, Vec::new(), Some("too_large")),
    }
}

impl<A: RepoApi + WorkspaceApi + Send + Sync + 'static> McpServer<A> {
    async fn upstream(
        &self,
        cx: &CallContext,
        args: UpstreamArgs,
    ) -> Result<CallToolResult, ToolError> {
        let mut limit = match args.limit.unwrap_or(UPSTREAM_DEFAULT) {
            n if (1..=UPSTREAM_MAX).contains(&n) => n,
            n => {
                return Err(ToolError::Invalid(format!(
                    "limit {n} is outside 1 to {UPSTREAM_MAX}"
                )));
            }
        };
        // Halve the bound until the result fits the cap.
        loop {
            let value = self.upstream_value(cx, &args, limit).await?;
            let result = CallToolResult::structured(value.clone());
            if fits(&result) || limit == 1 {
                return bounded(value);
            }
            limit /= 2;
        }
    }

    async fn upstream_value(
        &self,
        cx: &CallContext,
        args: &UpstreamArgs,
        limit: u32,
    ) -> Result<Value, ToolError> {
        if let Some(wc) = &self.working_copy {
            let req = WorkingCopyUpstream {
                repo: args.repo.clone(),
                workspace: args.workspace.clone(),
                limit,
                cursor: args.cursor.clone(),
            };
            if let Some(value) = wc.upstream(cx, req).await? {
                return Ok(value);
            }
        }
        let Some(name) = args.workspace.clone() else {
            return Err(ToolError::Invalid(
                "workspace is required outside a working copy".to_owned(),
            ));
        };
        self.drafts_upstream(cx, &args.repo, name, limit, args.cursor.clone())
            .await
            .map_err(ToolError::Api)
    }

    /// The server-side answer, from the workspace's drafts.
    async fn drafts_upstream(
        &self,
        cx: &CallContext,
        repo: &str,
        name: String,
        limit: u32,
        cursor: Option<String>,
    ) -> Result<Value, ApiError> {
        let wref = WorkspaceRef {
            repo: repo.to_owned(),
            name,
        };
        let ws = self.api.get_workspace(cx, wref.clone()).await?;
        let tip = self
            .api
            .resolve_ref(
                cx,
                ResolveRef {
                    repo: repo.to_owned(),
                    name: ws.line.clone(),
                },
            )
            .await?
            .target;
        let (base, head) = (ws.base, ws.head);
        let diff = |from, to, paths: Vec<RepoPath>, limit, token| Diff {
            repo: repo.to_owned(),
            from,
            to,
            paths,
            patch: None,
            page: Page { limit, token },
        };
        let mut out = json!({
            "line": ws.line,
            "workspace": ws.name,
            "base": base.to_string(),
            "head": head.map(|h| h.to_string()),
            "tip": tip.to_string(),
            "up_to_date": tip == base,
            "behind": 0,
            "behind_more": false,
            "changed": [],
            "next": null,
            "overlaps": [],
            "overlaps_truncated": false,
            "stack_conflict": null,
            "uncommitted_checked": false,
            "clean": true,
        });
        if tip == base {
            return Ok(out);
        }
        let history = self
            .api
            .log(
                cx,
                Log {
                    repo: repo.to_owned(),
                    from: tip,
                    until: Some(base),
                    paths: Vec::new(),
                    page: Page { limit, token: None },
                },
            )
            .await?;
        out["behind"] = json!(history.changesets.len());
        out["behind_more"] = json!(history.next.is_some());
        let page = self
            .api
            .diff(cx, diff(base, tip, Vec::new(), limit, cursor))
            .await?;
        out["changed"] = page.files.iter().map(|f| change_json(&f.change)).collect();
        out["next"] = json!(page.next);
        let Some(head) = head else {
            return Ok(out);
        };

        // The stack's paths.
        let mut own = BTreeSet::new();
        let mut token = None;
        loop {
            let page = self
                .api
                .diff(cx, diff(base, head, Vec::new(), 256, token))
                .await?;
            own.extend(page.files.into_iter().map(|f| f.change.path().clone()));
            match page.next {
                Some(next) if own.len() <= MAX_OWN_PATHS => token = Some(next),
                _ => break,
            }
        }
        let mut truncated = own.len() > MAX_OWN_PATHS;
        let own: Vec<RepoPath> = own.into_iter().take(MAX_OWN_PATHS).collect();

        // The line's changes to them.
        let mut theirs = Vec::new();
        'chunks: for chunk in own.chunks(DIFF_PATHS) {
            let wanted: BTreeSet<&RepoPath> = chunk.iter().collect();
            let mut token = None;
            loop {
                let page = self
                    .api
                    .diff(
                        cx,
                        diff(base, tip, chunk.to_vec(), DIFF_PATHS as u32, token),
                    )
                    .await?;
                for f in page.files {
                    if !wanted.contains(f.change.path()) {
                        continue;
                    }
                    if theirs.len() == limit as usize {
                        truncated = true;
                        break 'chunks;
                    }
                    theirs.push(f.change);
                }
                match page.next {
                    Some(next) => token = Some(next),
                    None => break,
                }
            }
        }

        // The engine's own sync planner, in memory.
        let planned = self
            .api
            .get_conflicts(
                cx,
                GetConflicts {
                    workspace: wref,
                    onto: Some(tip),
                    page: Page { limit, token: None },
                },
            )
            .await?;
        let stack: BTreeSet<RepoPath> = planned.paths.into_iter().map(|v| v.path).collect();

        let mut overlaps = Vec::new();
        let mut clean = planned.draft.is_none();
        for change in theirs {
            let path = change.path().clone();
            let b = self.read_side(cx, repo, base, &path).await?;
            let o = self.read_side(cx, repo, head, &path).await?;
            if o == b {
                continue;
            }
            let t = match change {
                Change::Delete { .. } => None,
                Change::Put { .. } => self.read_side(cx, repo, tip, &path).await?,
            };
            let (mut merges, hunks, mut reason) = predict(&b, &o, &t);
            if merges && stack.contains(&path) {
                merges = false;
                reason = Some("stack");
            }
            clean &= merges;
            overlaps.push(json!({
                "path": path.to_string(),
                "uncommitted": false,
                "outcome": if merges { "clean" } else { "conflict" },
                "hunks": hunks,
                "reason": reason,
            }));
        }
        out["overlaps"] = Value::Array(overlaps);
        out["overlaps_truncated"] = json!(truncated);
        out["stack_conflict"] = json!(planned.draft.map(|d: ChangesetId| d.to_string()));
        out["clean"] = json!(clean);
        Ok(out)
    }

    async fn read_side(
        &self,
        cx: &CallContext,
        repo: &str,
        at: ChangesetId,
        path: &RepoPath,
    ) -> Result<Side, ApiError> {
        let req = PathAt {
            repo: repo.to_owned(),
            at,
            path: path.clone(),
        };
        match self.api.read_file(cx, req).await {
            Ok(f) => Ok(Some((f.kind, f.data))),
            Err(ApiError::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

fn change_json(change: &Change) -> Value {
    json!({
        "path": change.path().to_string(),
        "status": match change {
            Change::Delete { .. } => "deleted",
            Change::Put { .. } => "changed",
        },
    })
}

#[tool_router(router = upstream_tool_router, vis = "pub(crate)")]
impl<A: RepoApi + WorkspaceApi + Send + Sync + 'static> McpServer<A> {
    #[tool(
        name = "graft_upstream",
        description = "Before landing, without moving anything: how far the line tip is ahead of the workspace's base, the files changed on the line since, and for each file you also changed whether the line merge would be clean or conflict (with the conflicting line ranges). Inside a working copy, uncommitted edits count.",
        annotations(
            title = "Upstream",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn graft_upstream(
        &self,
        Parameters(args): Parameters<UpstreamArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.upstream(&cx, args).await
        })
        .await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(s: &str) -> Side {
        Some((EntryKind::File, s.as_bytes().to_vec()))
    }

    #[test]
    fn predicts_like_the_line_merge() {
        let base = f("a\nb\nc\nd\ne\n");
        assert_eq!(
            predict(&base, &f("A\nb\nc\nd\ne\n"), &f("a\nb\nc\nd\nE\n")),
            (true, Vec::new(), None)
        );
        assert_eq!(
            predict(&base, &f("a\nB\nc\nd\ne\n"), &f("a\nX\nc\nd\ne\n")),
            (false, vec![[2, 2]], None)
        );
        assert_eq!(predict(&base, &None, &f("x\n")).2, Some("clash"));
    }
}
