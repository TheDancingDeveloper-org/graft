//! `/mcp`: the graft MCP server over streamable HTTP, for agents without a
//! checkout (`docs/design.md` §10.1, decision D17).
//!
//! Every request is authenticated here with the same bearer token as gRPC,
//! and its caller is handed to `graft-mcp` in the request's extensions: the
//! call context as a [`graft_mcp::Caller`], and the whole server [`Caller`]
//! (with the token's `jti`) as an opaque [`graft_mcp::Auth`]. The
//! tools reach the engine through [`McpApi`], which runs each operation
//! through [`App::call`] exactly as the gRPC services do: the same scopes,
//! the same free-space floor for writes, and the same metrics and spans.

use std::future::Future;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use graft_mcp::Auth;
use graft_model::ChangesetId;
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, RepoApi, WorkspaceApi};

use crate::app::{App, CallError, Caller, ServerEngine};
use crate::grpc::own_workspace;

/// The `/mcp` route, behind bearer-token authentication.
pub(crate) fn router(app: Arc<App>) -> Router {
    // The bearer token is what protects the endpoint, and the server is
    // reached by whatever name its proxy gives it, so rmcp's loopback-only
    // Host check is off.
    let config = graft_mcp::http_config().disable_allowed_hosts();
    let service = graft_mcp::streamable_http_service(Arc::new(McpApi(app.clone())), config);
    Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(app, authenticate))
}

/// Refuses a request without a valid token; otherwise records its caller.
async fn authenticate(State(app): State<Arc<App>>, mut request: Request, next: Next) -> Response {
    let headers = request.headers();
    let caller = app.authenticate(
        |name| headers.get(name).and_then(|v| v.to_str().ok()),
        false,
    );
    match caller {
        Ok(caller) => {
            let extensions = request.extensions_mut();
            extensions.insert(graft_mcp::Caller(caller.cx.clone()));
            extensions.insert(Auth(Arc::new(caller)));
            next.run(request).await
        }
        Err(e) => {
            app.refused("Mcp", &e);
            CallError::Api(e).into_response()
        }
    }
}

/// The API traits over [`App::call`].
pub(crate) struct McpApi(Arc<App>);

impl McpApi {
    async fn run<T, F, Fut>(
        &self,
        op: &'static str,
        repo: &str,
        cx: &CallContext,
        write: bool,
        f: F,
    ) -> ApiResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Arc<ServerEngine>, CallContext) -> Fut,
        Fut: Future<Output = ApiResult<T>> + Send + 'static,
    {
        // The request's server caller, set by `authenticate`: writes check
        // its `jti` against the denylist, as on gRPC.
        let auth = graft_mcp::request_auth()
            .and_then(|Auth(auth)| auth.downcast::<Caller>().ok())
            .ok_or(ApiError::Unauthenticated)?;
        let caller = Caller {
            cx: cx.clone(),
            exp_s: auth.exp_s,
            jti: auth.jti.clone(),
        };
        self.0
            .call(op, repo, caller, write, f)
            .await
            .map_err(|e| match e {
                CallError::Api(e) => e,
                CallError::Unimplemented(op) => ApiError::Rejected {
                    policy: "unimplemented".into(),
                    detail: format!("{op} is not implemented in this release"),
                },
                CallError::Internal => ApiError::Rejected {
                    policy: "internal".into(),
                    detail: "internal error".into(),
                },
            })
    }
}

/// One API method through [`McpApi::run`].
macro_rules! via {
    ($self:ident, $op:literal, $repo:expr, $cx:ident, $write:expr, |$e:ident, $c:ident| $body:expr) => {{
        let repo = $repo.clone();
        $self
            .run($op, &repo, $cx, $write, move |$e, $c| async move { $body })
            .await
    }};
}

impl RepoApi for McpApi {
    async fn create_changeset(&self, cx: &CallContext, req: CreateChangeset) -> ApiResult<Created> {
        // As on gRPC: a commit into a workspace moves its head, so only its
        // owner (or a repository admin) may.
        via!(self, "CreateChangeset", req.repo, cx, true, |e, c| {
            if let Some(name) = &req.workspace {
                own_workspace(&e, &c, &ws_ref(&req.repo, name)).await?;
            }
            e.create_changeset(&c, req).await
        })
    }
    async fn get_changesets(&self, cx: &CallContext, req: GetChangesets) -> ApiResult<Changesets> {
        via!(self, "GetChangesets", req.repo, cx, false, |e, c| e
            .get_changesets(&c, req)
            .await)
    }
    async fn resolve_ref(&self, cx: &CallContext, req: ResolveRef) -> ApiResult<RefView> {
        via!(self, "ResolveRef", req.repo, cx, false, |e, c| e
            .resolve_ref(&c, req)
            .await)
    }
    async fn list_refs(&self, cx: &CallContext, req: ListRefs) -> ApiResult<Vec<RefView>> {
        via!(self, "ListRefs", req.repo, cx, false, |e, c| e
            .list_refs(&c, req)
            .await)
    }
    async fn update_ref(&self, cx: &CallContext, req: UpdateRef) -> ApiResult<RefView> {
        via!(self, "UpdateRef", req.repo, cx, true, |e, c| e
            .update_ref(&c, req)
            .await)
    }
    async fn create_line(&self, cx: &CallContext, req: CreateLine) -> ApiResult<RefView> {
        via!(self, "CreateLine", req.repo, cx, true, |e, c| e
            .create_line(&c, req)
            .await)
    }
    async fn stat(&self, cx: &CallContext, req: PathAt) -> ApiResult<EntryView> {
        via!(self, "Stat", req.repo, cx, false, |e, c| e
            .stat(&c, req)
            .await)
    }
    async fn list_tree(&self, cx: &CallContext, req: ListTree) -> ApiResult<Entries> {
        via!(self, "ListTree", req.repo, cx, false, |e, c| e
            .list_tree(&c, req)
            .await)
    }
    async fn read_file(&self, cx: &CallContext, req: PathAt) -> ApiResult<FileContent> {
        via!(self, "ReadFile", req.repo, cx, false, |e, c| e
            .read_file(&c, req)
            .await)
    }
    async fn log(&self, cx: &CallContext, req: Log) -> ApiResult<History> {
        via!(self, "Log", req.repo, cx, false, |e, c| e
            .log(&c, req)
            .await)
    }
    async fn is_ancestor(&self, cx: &CallContext, req: Pair) -> ApiResult<bool> {
        via!(self, "IsAncestor", req.repo, cx, false, |e, c| e
            .is_ancestor(&c, req)
            .await)
    }
    async fn merge_base(&self, cx: &CallContext, req: Pair) -> ApiResult<Option<ChangesetId>> {
        via!(self, "MergeBase", req.repo, cx, false, |e, c| e
            .merge_base(&c, req)
            .await)
    }
    async fn diff(&self, cx: &CallContext, req: Diff) -> ApiResult<DiffResult> {
        via!(self, "Diff", req.repo, cx, false, |e, c| e
            .diff(&c, req)
            .await)
    }
    async fn blame(&self, cx: &CallContext, req: Blame) -> ApiResult<BlameResult> {
        via!(self, "Blame", req.repo, cx, false, |e, c| e
            .blame(&c, req)
            .await)
    }
    async fn rebase(&self, cx: &CallContext, req: Rebase) -> ApiResult<ChangesetId> {
        // A dry run writes nothing, so it is not admitted as a write.
        let write = !req.dry_run;
        via!(self, "Rebase", req.repo, cx, write, |e, c| e
            .rebase(&c, req)
            .await)
    }
    async fn revert(&self, cx: &CallContext, req: Revert) -> ApiResult<ChangesetId> {
        // As on gRPC: a revert into a workspace adds a draft to it, so only
        // its owner (or a repository admin) may.
        via!(self, "Revert", req.repo, cx, true, |e, c| {
            if let Some(name) = &req.workspace {
                own_workspace(&e, &c, &ws_ref(&req.repo, name)).await?;
            }
            e.revert(&c, req).await
        })
    }
    async fn land(&self, cx: &CallContext, req: Land) -> ApiResult<Landed> {
        via!(self, "Land", req.repo, cx, true, |e, c| e
            .land(&c, req)
            .await)
    }
    async fn line_merge(&self, cx: &CallContext, req: LineMerge) -> ApiResult<LineMergeOutcome> {
        via!(self, "LineMerge", req.repo, cx, true, |e, c| e
            .line_merge(&c, req)
            .await)
    }
    async fn line_merges(&self, cx: &CallContext, req: LineMerges) -> ApiResult<LineMergePage> {
        via!(self, "LineMerges", req.repo, cx, false, |e, c| e
            .line_merges(&c, req)
            .await)
    }
    async fn attest(&self, cx: &CallContext, req: Attest) -> ApiResult<()> {
        via!(self, "Attest", req.repo, cx, true, |e, c| e
            .attest(&c, req)
            .await)
    }
    async fn watch(&self, cx: &CallContext, req: Watch) -> ApiResult<JournalPage> {
        // Never a long poll here: the MCP tools read the journal only in
        // pages.
        let req = Watch { wait_ms: 0, ..req };
        via!(self, "Watch", req.repo, cx, false, |e, c| e
            .watch(&c, req)
            .await)
    }
}

/// The workspace `name` in `repo`.
fn ws_ref(repo: &str, name: &str) -> WorkspaceRef {
    WorkspaceRef {
        repo: repo.to_owned(),
        name: name.to_owned(),
    }
}

// As on gRPC: a workspace is its owner's (`docs/design.md` §7.2). Every
// operation on a named workspace, reads included, runs `own_workspace`
// first, so another principal learns nothing of it.
impl WorkspaceApi for McpApi {
    async fn open_workspace(
        &self,
        cx: &CallContext,
        req: OpenWorkspace,
    ) -> ApiResult<WorkspaceView> {
        via!(self, "OpenWorkspace", req.repo, cx, true, |e, c| e
            .open_workspace(&c, req)
            .await)
    }
    async fn get_workspace(&self, cx: &CallContext, req: WorkspaceRef) -> ApiResult<WorkspaceView> {
        via!(self, "GetWorkspace", req.repo, cx, false, |e, c| {
            own_workspace(&e, &c, &req).await?;
            e.get_workspace(&c, req).await
        })
    }
    async fn list_workspaces(
        &self,
        cx: &CallContext,
        repo: String,
        page: Page,
    ) -> ApiResult<Vec<WorkspaceView>> {
        via!(self, "ListWorkspaces", repo, cx, false, |e, c| e
            .list_workspaces(&c, repo, page)
            .await)
    }
    async fn renew_lease(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        lease_ms: u64,
    ) -> ApiResult<WorkspaceView> {
        via!(self, "RenewLease", req.repo, cx, true, |e, c| {
            own_workspace(&e, &c, &req).await?;
            e.renew_lease(&c, req, lease_ms).await
        })
    }
    async fn set_head(&self, cx: &CallContext, req: SetHead) -> ApiResult<WorkspaceView> {
        via!(self, "SetHead", req.workspace.repo, cx, true, |e, c| {
            own_workspace(&e, &c, &req.workspace).await?;
            e.set_head(&c, req).await
        })
    }
    async fn sync(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        onto: Option<ChangesetId>,
    ) -> ApiResult<Synced> {
        via!(self, "Sync", req.repo, cx, true, |e, c| {
            own_workspace(&e, &c, &req).await?;
            e.sync(&c, req, onto).await
        })
    }
    async fn stack(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        op: StackOp,
    ) -> ApiResult<WorkspaceView> {
        via!(self, "Stack", req.repo, cx, true, |e, c| {
            own_workspace(&e, &c, &req).await?;
            e.stack(&c, req, op).await
        })
    }
    async fn close_workspace(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        keep_drafts: bool,
    ) -> ApiResult<()> {
        via!(self, "CloseWorkspace", req.repo, cx, true, |e, c| {
            own_workspace(&e, &c, &req).await?;
            e.close_workspace(&c, req, keep_drafts).await
        })
    }

    async fn get_conflicts(&self, cx: &CallContext, req: GetConflicts) -> ApiResult<Conflicts> {
        via!(
            self,
            "GetConflicts",
            req.workspace.repo,
            cx,
            true,
            |e, c| {
                own_workspace(&e, &c, &req.workspace).await?;
                e.get_conflicts(&c, req).await
            }
        )
    }
    async fn overlapping_drafts(
        &self,
        cx: &CallContext,
        req: OverlappingDrafts,
    ) -> ApiResult<DraftOverlap> {
        // Other principals' workspaces, so no ownership check: the engine
        // requires the `repo:<r>:drafts` scope instead.
        via!(self, "OverlappingDrafts", req.repo, cx, false, |e, c| e
            .overlapping_drafts(&c, req)
            .await)
    }
    async fn resolve(&self, cx: &CallContext, req: Resolve) -> ApiResult<Resolved> {
        via!(self, "Resolve", req.workspace.repo, cx, true, |e, c| {
            own_workspace(&e, &c, &req.workspace).await?;
            e.resolve(&c, req).await
        })
    }
}
