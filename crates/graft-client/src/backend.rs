//! A client backed by either gRPC or an engine opened in this process.

use std::sync::Arc;

use graft_model::{ChangesetId, ObjectId};
use graft_proto::api::{ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::context::CallContext;
use graft_proto::error::ApiResult;
use graft_proto::types::*;

use crate::local::LocalEngine;
use crate::{Endpoint, Remote};

/// The API used by a command: remote transport or local engine.
#[derive(Clone)]
pub enum Backend {
    /// gRPC to a server or node.
    Remote(Arc<Remote>),
    /// An in-process repository engine.
    Local {
        /// Engine opened for this process.
        engine: Arc<LocalEngine>,
        /// Repository name.
        repo: String,
    },
}

impl Backend {
    /// Wraps a remote client.
    pub fn remote(remote: Remote) -> Self {
        Self::Remote(Arc::new(remote))
    }

    /// Uses an engine opened in this process for `repo`.
    pub fn local(engine: Arc<LocalEngine>, repo: impl Into<String>) -> Self {
        Self::Local {
            engine,
            repo: repo.into(),
        }
    }

    /// The remote endpoint, if this is a remote backend.
    pub fn endpoint(&self) -> Option<&Endpoint> {
        match self {
            Self::Remote(remote) => Some(remote.endpoint()),
            Self::Local { .. } => None,
        }
    }

    fn local_context(repo: &str, cx: &CallContext) -> CallContext {
        crate::LocalRepo::context(repo, cx.request_id.clone())
    }
}

macro_rules! forward {
    ($self:ident.$method:ident($cx:ident $(, $arg:expr)*)) => {
        match $self {
            Backend::Remote(api) => api.$method($cx $(, $arg)*).await,
            Backend::Local { engine: api, repo } => {
                let local = Backend::local_context(repo, $cx);
                api.$method(&local $(, $arg)*).await
            },
        }
    };
}

impl ObjectApi for Backend {
    async fn get_objects(&self, cx: &CallContext, req: GetObjects) -> ApiResult<Objects> {
        forward!(self.get_objects(cx, req))
    }
    async fn has_objects(&self, cx: &CallContext, req: HasObjects) -> ApiResult<Vec<ObjectId>> {
        forward!(self.has_objects(cx, req))
    }
    async fn put_objects(&self, cx: &CallContext, req: PutObjects) -> ApiResult<Vec<ObjectId>> {
        forward!(self.put_objects(cx, req))
    }
}

/// One `RepoApi` method of [`Backend`], through `forward!`.
macro_rules! forward_repo {
    ($($name:ident($req:ty) -> $out:ty;)*) => {$(
        async fn $name(&self, cx: &CallContext, req: $req) -> ApiResult<$out> {
            forward!(self.$name(cx, req))
        }
    )*};
}

impl RepoApi for Backend {
    graft_proto::forward_repo_api!(forward_repo);
}

impl WorkspaceApi for Backend {
    async fn open_workspace(
        &self,
        cx: &CallContext,
        req: OpenWorkspace,
    ) -> ApiResult<WorkspaceView> {
        forward!(self.open_workspace(cx, req))
    }
    async fn get_workspace(&self, cx: &CallContext, req: WorkspaceRef) -> ApiResult<WorkspaceView> {
        forward!(self.get_workspace(cx, req))
    }
    async fn list_workspaces(
        &self,
        cx: &CallContext,
        repo: String,
        page: Page,
    ) -> ApiResult<Vec<WorkspaceView>> {
        forward!(self.list_workspaces(cx, repo, page))
    }
    async fn renew_lease(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        lease_ms: u64,
    ) -> ApiResult<WorkspaceView> {
        forward!(self.renew_lease(cx, req, lease_ms))
    }
    async fn set_head(&self, cx: &CallContext, req: SetHead) -> ApiResult<WorkspaceView> {
        forward!(self.set_head(cx, req))
    }
    async fn sync(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        onto: Option<ChangesetId>,
    ) -> ApiResult<Synced> {
        forward!(self.sync(cx, req, onto))
    }
    async fn stack(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        op: StackOp,
    ) -> ApiResult<WorkspaceView> {
        forward!(self.stack(cx, req, op))
    }
    async fn close_workspace(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        keep_drafts: bool,
    ) -> ApiResult<()> {
        forward!(self.close_workspace(cx, req, keep_drafts))
    }
    async fn get_conflicts(&self, cx: &CallContext, req: GetConflicts) -> ApiResult<Conflicts> {
        forward!(self.get_conflicts(cx, req))
    }
    async fn resolve(&self, cx: &CallContext, req: Resolve) -> ApiResult<Resolved> {
        forward!(self.resolve(cx, req))
    }
    async fn overlapping_drafts(
        &self,
        cx: &CallContext,
        req: OverlappingDrafts,
    ) -> ApiResult<DraftOverlap> {
        forward!(self.overlapping_drafts(cx, req))
    }
}
