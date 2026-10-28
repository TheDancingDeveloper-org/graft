//! A client backed by either gRPC or an engine opened in this process.

use std::sync::Arc;

use graft_model::ObjectId;
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

/// One `WorkspaceApi` method of [`Backend`], through `forward!`.
macro_rules! forward_workspace {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $out:ty;)*) => {$(
        async fn $name(&self, cx: &CallContext, $($arg: $ty),*) -> ApiResult<$out> {
            forward!(self.$name(cx $(, $arg)*))
        }
    )*};
}

impl WorkspaceApi for Backend {
    graft_proto::forward_workspace_api!(forward_workspace);
}
