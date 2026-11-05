//! `graft_repo_create` (D41): creates a repository through `CreateRepo`.
//! It needs `repos:admin`, after which the creator administers the
//! repository, or the repository's own admin.
//!
//! The API traits the server is generic over carry no admin calls, so the
//! launcher installs a [`RepoAdmin`] (`graft mcp` does, over its connection).
//! Without one the tool answers that it is not available here.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use graft_proto::{
    CallContext,
    api::{RepoApi, WorkspaceApi},
    error::ApiResult,
};
use rmcp::{
    ErrorData, RoleServer, handler::server::wrapper::Parameters, model::CallToolResult,
    service::RequestContext, tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::render::{ToolError, bounded};
use crate::server::{McpServer, respond};

/// The future a [`RepoAdmin`] call returns.
pub type RepoAdminFuture<'a> = Pin<Box<dyn Future<Output = ApiResult<()>> + Send + 'a>>;

/// Repository administration the launcher provides.
pub trait RepoAdmin: Send + Sync + 'static {
    /// `CreateRepo`: creates `name`; a repository that exists is a success.
    fn create_repo<'a>(&'a self, cx: &'a CallContext, name: String) -> RepoAdminFuture<'a>;
}

/// Arguments of `graft_repo_create`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoCreateArgs {
    /// The new repository's name.
    pub repo: String,
}

/// Longest repository name the tool passes on; the API applies its own
/// naming rules after this.
const NAME_MAX_BYTES: usize = 256;

impl<A: RepoApi + WorkspaceApi + Send + Sync + 'static> McpServer<A> {
    /// Serves `graft_repo_create` through `admin`.
    #[must_use]
    pub fn with_repo_admin(mut self, admin: Arc<dyn RepoAdmin>) -> Self {
        self.repo_admin = Some(admin);
        self
    }

    async fn repo_create(
        &self,
        cx: &CallContext,
        args: RepoCreateArgs,
    ) -> Result<CallToolResult, ToolError> {
        if args.repo.is_empty()
            || args.repo.len() > NAME_MAX_BYTES
            || args.repo.chars().any(char::is_control)
        {
            return Err(ToolError::Invalid(
                "`repo` must be a short, printable repository name".to_owned(),
            ));
        }
        let Some(admin) = &self.repo_admin else {
            return Err(ToolError::Invalid(
                "graft_repo_create is not available on this server".to_owned(),
            ));
        };
        admin
            .create_repo(cx, args.repo.clone())
            .await
            .map_err(ToolError::Api)?;
        bounded(json!({ "repo": args.repo }))
    }
}

#[tool_router(router = repos_tool_router, vis = "pub(crate)")]
impl<A: RepoApi + WorkspaceApi + Send + Sync + 'static> McpServer<A> {
    #[tool(
        name = "graft_repo_create",
        description = "Create a repository. Needs repos:admin (the creator then administers it) or the repository's admin. A repository that already exists is a success.",
        annotations(
            title = "Create repository",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn graft_repo_create(
        &self,
        Parameters(args): Parameters<RepoCreateArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.repo_create(&cx, args).await
        })
        .await)
    }
}
