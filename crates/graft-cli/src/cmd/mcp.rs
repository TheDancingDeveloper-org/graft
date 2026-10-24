//! `graft mcp`: the graft MCP server (`graft-mcp`) over standard input and
//! output, against the remote API (`docs/design.md` §10.1, D17).
//!
//! Standard output is the MCP channel, so anything for people goes to
//! standard error. The endpoint and token come from the same places as every
//! command (`--endpoint`, `GRAFT_ENDPOINT`, the node socket, the working
//! copy's server; `--token-file` or `GRAFT_TOKEN`), and the token is never
//! printed.
//!
//! Started inside a working copy it also serves the working-copy tools:
//! `graft_status`, `graft_commit` with `from_working_copy`, and
//! `graft_land` and `graft_sync` on the working copy's workspace, which move
//! the working copy with the server. Outside one it serves the repository
//! tools, and every tool names its repository.

use std::sync::Arc;

use graft_client::fetch::FetchOptions;
use graft_client::workcopy::{
    CommitLimits, CommitOptions, DEFAULT_CACHE_BYTES, LandOptions, LandOutcome, WorkingCopy,
};
use graft_client::workspace::{DEFAULT_LEASE_MS, HEARTBEAT_EVERY, with_heartbeat};
use graft_client::{Endpoint, Error as ClientError, Remote, discover};
use graft_mcp::{
    McpServer, WorkingCopyCommit, WorkingCopyError, WorkingCopyFuture, WorkingCopyLand,
    WorkingCopySync, WorkingCopyTools, WorkingCopyUpstream,
};
use graft_model::{Author, AuthorKind, Metadata, Timestamp};
use graft_proto::CallContext;
use graft_proto::types::WorkspaceRef;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::cli::Global;
use crate::cmd::status::{change_json, upstream_json};
use crate::conn::{call_context, read_token, runtime, working_copy};
use crate::error::Failure;
use crate::identity;
use crate::render;

/// Paths listed in a status or commit result before it says `truncated`.
const PATHS_MAX: usize = 500;

/// Runs the MCP server over stdio until the client closes it.
pub(crate) fn run(g: &Global) -> Result<(), Failure> {
    let wc = working_copy()?;
    runtime()?.block_on(async {
        let remote = Arc::new(connect(g, wc.as_ref()).await?);
        let principal = call_context().principal;
        let mut server = McpServer::new(Arc::clone(&remote), principal);
        if let Some(wc) = wc {
            server = server.with_working_copy(Arc::new(Served {
                repo: wc.config().repo.clone(),
                wc: Mutex::new(wc),
                remote,
            }));
        }
        server.serve_stdio().await?;
        Ok(())
    })
}

/// Connects to the endpoint the other commands would use. Unlike them it
/// needs no repository: every tool names its own.
async fn connect(g: &Global, wc: Option<&WorkingCopy>) -> Result<Remote, Failure> {
    let endpoint = match &g.endpoint {
        Some(text) => Endpoint::parse(text),
        None => {
            let configured = wc.and_then(|wc| wc.config().endpoint.clone());
            discover(configured.as_deref()).ok_or_else(|| {
                Failure::usage(
                    "no endpoint: pass --endpoint, set GRAFT_ENDPOINT, or run inside a working copy",
                )
            })?
        }
    };
    let remote = Remote::connect(endpoint).await?;
    Ok(match read_token(g)? {
        Some(token) => remote.with_token(token),
        None => remote,
    })
}

/// The working copy `graft mcp` started in, served to the MCP tools. One
/// operation at a time holds it.
struct Served {
    repo: String,
    wc: Mutex<WorkingCopy>,
    remote: Arc<Remote>,
}

impl Served {
    fn check_repo(&self, repo: &str) -> Result<(), WorkingCopyError> {
        if repo == self.repo {
            Ok(())
        } else {
            Err(WorkingCopyError::Invalid(format!(
                "this working copy is of repository {:?}, not {repo:?}",
                self.repo
            )))
        }
    }

    /// Whether `repo` and `workspace` name this working copy's workspace.
    fn owns(&self, wc: &WorkingCopy, repo: &str, workspace: &str) -> bool {
        repo == self.repo && wc.config().workspace.as_deref() == Some(workspace)
    }

    /// Renews (or opens) the workspace and finishes an interrupted commit,
    /// as every write command does first. With `settle`, an unfinished
    /// update is finished too.
    async fn begin(
        &self,
        wc: &mut WorkingCopy,
        cx: &CallContext,
        settle: bool,
    ) -> Result<Value, WorkingCopyError> {
        let begun = wc
            .begin(&*self.remote, cx, DEFAULT_LEASE_MS)
            .await
            .map_err(client)?;
        if settle && begun.unfinished {
            let cache = wc.open_cache(DEFAULT_CACHE_BYTES).map_err(client)?;
            let moved = wc
                .finish(
                    &*self.remote,
                    &cache,
                    cx,
                    DEFAULT_LEASE_MS,
                    &FetchOptions::default(),
                )
                .await
                .map_err(client)?;
            if let Some(conflicts) = moved.and_then(|m| m.conflicts) {
                return Err(client(ClientError::Conflicted(Box::new(conflicts))));
            }
        }
        Ok(match begun.recovered {
            None => Value::Null,
            Some(graft_client::workcopy::Recovered::Finished(_)) => "finished".into(),
            Some(graft_client::workcopy::Recovered::RolledBack) => "rolled_back".into(),
        })
    }

    fn workspace_ref(&self, wc: &WorkingCopy) -> WorkspaceRef {
        WorkspaceRef {
            repo: self.repo.clone(),
            name: wc.config().workspace.clone().unwrap_or_default(),
        }
    }
}

impl WorkingCopyTools for Served {
    fn upstream<'a>(
        &'a self,
        cx: &'a CallContext,
        req: WorkingCopyUpstream,
    ) -> WorkingCopyFuture<'a, Option<Value>> {
        Box::pin(async move {
            let wc = self.wc.lock().await;
            let own = wc.config().workspace.clone();
            if req.repo != self.repo
                || req
                    .workspace
                    .as_ref()
                    .is_some_and(|w| Some(w) != own.as_ref())
            {
                return Ok(None);
            }
            let up = wc
                .upstream(&*self.remote, cx, req.limit, req.cursor)
                .await
                .map_err(client)?;
            Ok(Some(upstream_json(&up, own.as_deref())))
        })
    }

    fn status<'a>(&'a self, repo: &'a str) -> WorkingCopyFuture<'a, Value> {
        Box::pin(async move {
            self.check_repo(repo)?;
            let wc = self.wc.lock().await;
            let changes = wc.status().map_err(local)?;
            let conflicts = wc.open_conflicts().map_err(client)?;
            let state = wc.state();
            Ok(json!({
                "repo": self.repo,
                "workspace": wc.config().workspace,
                "line": state.line,
                "base": state.base.to_string(),
                "head": state.head.map(|h| h.to_string()),
                "clean": changes.is_empty(),
                "change_count": changes.len(),
                "changes": changes.iter().take(PATHS_MAX).map(change_json).collect::<Vec<_>>(),
                "truncated": changes.len() > PATHS_MAX,
                "conflicts": conflicts.iter().take(PATHS_MAX).map(|f| json!({
                    "path": f.path.to_string(),
                    "mode": f.mode.name(),
                })).collect::<Vec<_>>(),
                "unfinished": state.moving.is_some(),
            }))
        })
    }

    fn commit<'a>(
        &'a self,
        cx: &'a CallContext,
        req: WorkingCopyCommit,
    ) -> WorkingCopyFuture<'a, Value> {
        Box::pin(async move {
            self.check_repo(&req.repo)?;
            let mut wc = self.wc.lock().await;
            match wc.config().workspace.clone() {
                Some(name) if name != req.workspace => {
                    return Err(WorkingCopyError::Invalid(format!(
                        "this working copy's workspace is {name:?}, not {:?}",
                        req.workspace
                    )));
                }
                Some(_) => {}
                None => wc
                    .set_workspace(Some(req.workspace.clone()))
                    .map_err(client)?,
            }
            let recovered = self.begin(&mut wc, cx, true).await?;
            let opts = CommitOptions {
                message: Some(req.message),
                author: author(),
                authored_at: Timestamp {
                    ms: req.authored_at_ms.unwrap_or_else(now_ms),
                    offset_min: 0,
                },
                metadata: {
                    let mut m = Metadata::new();
                    identity::add_vogt_metadata(&mut m);
                    m
                },
                amend: false,
                limits: CommitLimits::default(),
            };
            let ws = self.workspace_ref(&wc);
            let done = with_heartbeat(
                &*self.remote,
                cx,
                ws,
                DEFAULT_LEASE_MS,
                HEARTBEAT_EVERY,
                wc.commit(&*self.remote, cx, &opts),
            )
            .await
            .map_err(client)?;
            Ok(json!({
                "id": done.id.to_string(),
                "parent": done.parent.to_string(),
                "workspace": done.workspace,
                "change_count": done.changes.len(),
                "changes": done.changes.iter().take(PATHS_MAX).map(render::change).collect::<Vec<_>>(),
                "truncated": done.changes.len() > PATHS_MAX,
                "upload": {
                    "blobs": done.upload.blobs,
                    "uploaded": done.upload.uploaded,
                    "bytes": done.upload.bytes,
                    "requests": done.upload.requests,
                },
                "recovered": recovered,
            }))
        })
    }

    fn land<'a>(
        &'a self,
        cx: &'a CallContext,
        req: WorkingCopyLand,
    ) -> WorkingCopyFuture<'a, Option<Value>> {
        Box::pin(async move {
            let mut wc = self.wc.lock().await;
            if !self.owns(&wc, &req.repo, &req.workspace) {
                return Ok(None);
            }
            let state = wc.state();
            if req.line.as_ref().is_some_and(|l| *l != state.line) {
                return Err(WorkingCopyError::Invalid(format!(
                    "this working copy lands on its own line, {:?}",
                    state.line
                )));
            }
            if req
                .head
                .as_ref()
                .is_some_and(|h| Some(h.as_str()) != state.head.map(|id| id.to_string()).as_deref())
            {
                return Err(WorkingCopyError::Invalid(
                    "this working copy lands its own head; omit `head`".into(),
                ));
            }
            let recovered = self.begin(&mut wc, cx, false).await?;
            let cache = wc.open_cache(DEFAULT_CACHE_BYTES).map_err(client)?;
            let opts = LandOptions {
                squash: req.squash,
                merge: req.merge,
                merged_from: req.merged_from.clone(),
                ..LandOptions::default()
            };
            let ws = self.workspace_ref(&wc);
            let out = with_heartbeat(
                &*self.remote,
                cx,
                ws,
                DEFAULT_LEASE_MS,
                HEARTBEAT_EVERY,
                wc.land(&*self.remote, &cache, cx, &opts, &FetchOptions::default()),
            )
            .await
            .map_err(client)?;
            let line = wc.state().line.clone();
            let value = match out {
                LandOutcome::Landed {
                    landed,
                    rebased,
                    update,
                } => json!({
                    "outcome": "landed",
                    "line": line,
                    "landed": landed.to_string(),
                    "rebased": rebased,
                    "files_updated": update.changed,
                    "recovered": recovered,
                }),
                LandOutcome::Queued { pr_url } => json!({
                    "outcome": "queued",
                    "line": line,
                    "pr_url": pr_url,
                    "detail": "handed to the PR bridge; the drafts land when the pull request merges",
                    "recovered": recovered,
                }),
                _ => json!({"outcome": "unknown", "line": line}),
            };
            Ok(Some(value))
        })
    }

    fn sync<'a>(
        &'a self,
        cx: &'a CallContext,
        req: WorkingCopySync,
    ) -> WorkingCopyFuture<'a, Option<Value>> {
        Box::pin(async move {
            let mut wc = self.wc.lock().await;
            if !self.owns(&wc, &req.repo, &req.workspace) {
                return Ok(None);
            }
            if req.onto.is_some() {
                return Err(WorkingCopyError::Invalid(
                    "a working copy syncs to its line tip; omit `onto`".into(),
                ));
            }
            let recovered = self.begin(&mut wc, cx, false).await?;
            let cache = wc.open_cache(DEFAULT_CACHE_BYTES).map_err(client)?;
            let out = wc
                .sync(
                    &*self.remote,
                    &cache,
                    cx,
                    DEFAULT_LEASE_MS,
                    &FetchOptions::default(),
                )
                .await
                .map_err(client)?;
            Ok(Some(json!({
                "from": out.from.to_string(),
                "base": out.base.to_string(),
                "head": out.head.map(|h| h.to_string()),
                "files_updated": out.update.changed,
                "recovered": recovered,
            })))
        })
    }
}

/// A working-copy failure as the MCP server reports it: API errors stay
/// typed; the rest keep the kind and message the CLI would print.
fn client(e: ClientError) -> WorkingCopyError {
    match e {
        ClientError::Api(e) => WorkingCopyError::Api(e),
        other => local(other),
    }
}

fn local(e: impl Into<Failure>) -> WorkingCopyError {
    let f: Failure = e.into();
    WorkingCopyError::Local {
        kind: f.kind,
        message: f.message,
    }
}

/// The author of working-copy drafts: the default identity, with kind
/// `GRAFT_AUTHOR_KIND` (default `agent`, since MCP clients are agents).
fn author() -> Author {
    let kind =
        identity::parse_kind(&identity::default_kind_text("agent")).unwrap_or(AuthorKind::Agent);
    identity::default_author(kind)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
