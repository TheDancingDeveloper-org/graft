//! Mirror settings and batch requests (D41 §3.2, §3.5): `graft_mirror_*`
//! and `graft_batch_*`.
//!
//! The calls live on the server's mirror service, which the API traits the
//! server is generic over don't carry, so the launcher installs a
//! [`MirrorAdmin`] (`graft mcp` does, over its connection). Without one the
//! tools answer that they are not available here.
//!
//! Binding, removing and purging a mirror need `mirror:bind`, which only
//! an operator holds; an agent calling them gets the server's typed
//! permission refusal. No tool takes or returns a credential: a row says
//! only whether one is set. Requests that create something carry a
//! `client_request_id`, made once per call and reused on a retry, so a
//! retried cut returns the request the first attempt recorded.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use graft_model::{ChangesetId, ObjectId};
use graft_proto::{
    CallContext,
    api::{RepoApi, WorkspaceApi},
    error::{ApiError, ApiResult},
    pb,
};
use rmcp::{
    ErrorData, RoleServer, handler::server::wrapper::Parameters, model::CallToolResult,
    service::RequestContext, tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::render::{ToolError, bounded, fit};
use crate::server::{McpServer, respond};

/// The future a [`MirrorAdmin`] call returns.
pub type MirrorFuture<'a, T> = Pin<Box<dyn Future<Output = ApiResult<T>> + Send + 'a>>;

/// The mirror service calls the launcher provides. Each is one RPC; the
/// server checks the grants.
pub trait MirrorAdmin: Send + Sync + 'static {
    /// `SetMirror`: creates a row (no expected version) or replaces one.
    fn set_mirror<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::SetMirrorRequest,
    ) -> MirrorFuture<'a, pb::Mirror>;
    /// `GetMirror`: one row.
    fn get_mirror<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::MirrorKey,
    ) -> MirrorFuture<'a, pb::Mirror>;
    /// `ListMirrors`: one page of rows, of a repository or of all.
    fn list_mirrors<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::ListMirrorsRequest,
    ) -> MirrorFuture<'a, pb::MirrorPage>;
    /// `RemoveMirror`: deletes a row, optionally clearing its markers.
    fn remove_mirror<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::RemoveMirrorRequest,
    ) -> MirrorFuture<'a, pb::RemoveMirrorResult>;
    /// `PurgeMirror`: records a purge request.
    fn purge_mirror<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::PurgeMirrorRequest,
    ) -> MirrorFuture<'a, pb::MirrorRequest>;
    /// `SetMirrorCredential`: seals a credential into a row. Write-only.
    fn set_mirror_credential<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::SetMirrorCredentialRequest,
    ) -> MirrorFuture<'a, pb::Mirror>;
    /// `CutBatch`: records a batch request, or returns the one recorded
    /// under the same `client_request_id`.
    fn cut_batch<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::CutBatchRequest,
    ) -> MirrorFuture<'a, pb::MirrorRequest>;
    /// `GetBatch`: the open batch of a line.
    fn get_batch<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::BatchKey,
    ) -> MirrorFuture<'a, pb::BatchView>;
    /// `ListBatches`: one page of a repository's batches.
    fn list_batches<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::ListBatchesRequest,
    ) -> MirrorFuture<'a, pb::BatchPage>;
}

/// Attempts a request gets when the server is unavailable, the first
/// included.
const ATTEMPTS: u32 = 3;
/// Longest wait between attempts, whatever the server suggests.
const MAX_RETRY_WAIT_MS: u64 = 2_000;

/// A fresh idempotency key for one user action: `<origin>-<time>-<pid>-<n>`.
/// Made once and reused for every attempt of that action.
#[must_use]
pub fn new_client_request_id(origin: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{origin}-{nanos:x}-{:x}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Runs `call` until it answers something other than `unavailable`, at
/// most [`ATTEMPTS`] times. Every attempt sends the same request, so the
/// caller builds it (with its `client_request_id`) once, outside.
pub async fn with_retry<T, F, Fut>(mut call: F) -> ApiResult<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ApiResult<T>>,
{
    let mut attempt = 1;
    loop {
        match call().await {
            Err(ApiError::Unavailable { retry_after_ms }) if attempt < ATTEMPTS => {
                attempt += 1;
                let wait = retry_after_ms.clamp(10, MAX_RETRY_WAIT_MS);
                tokio::time::sleep(Duration::from_millis(wait)).await;
            }
            other => return other,
        }
    }
}

/// A 32-byte id on the wire, in its text form; anything else as hex.
fn id_text(bytes: &[u8]) -> String {
    match <[u8; 32]>::try_from(bytes) {
        Ok(a) => ChangesetId(ObjectId(a)).to_string(),
        Err(_) => bytes.iter().map(|b| format!("{b:02x}")).collect(),
    }
}

/// Parses a changeset id for the wire.
pub fn changeset_bytes(field: &str, text: &str) -> Result<Vec<u8>, String> {
    ChangesetId::parse(text)
        .map(|id| id.0.0.to_vec())
        .map_err(|_| format!("{field} `{text}` is not a full changeset id"))
}

/// The lowercase name of an enum value, without its type prefix.
fn enum_name(full: &str, prefix: &str) -> String {
    full.strip_prefix(prefix)
        .unwrap_or(full)
        .to_ascii_lowercase()
}

/// The JSON form of a mirror row. It carries whether a credential is set
/// and the key id that sealed it, never the credential.
#[must_use]
pub fn mirror_json(m: &pb::Mirror) -> Value {
    let export = m.export.clone().unwrap_or_default();
    json!({
        "repo": m.repo,
        "upstream": m.upstream,
        "lines": m.lines.iter().map(|l| json!({
            "branch": l.branch,
            "line": l.line,
            "direction": l.direction,
        })).collect::<Vec<_>>(),
        "interval_s": m.interval_s,
        "enabled": m.enabled,
        "credential_set": m.credential_set,
        "kek_id": m.kek_id,
        "export": {
            "default_author": export.default_author,
            "authors": export.authors.iter().map(|a| json!({
                "principal": a.principal,
                "identity": a.identity,
            })).collect::<Vec<_>>(),
        },
        "version": m.version,
        "status": m.status.as_ref().map(status_json),
        "created_by": m.created_by,
        "updated_by": m.updated_by,
        "updated_at_ms": m.updated_at_ms,
    })
}

fn status_json(s: &pb::MirrorStatus) -> Value {
    let code = pb::MirrorErrorCode::try_from(s.error_code).map_or_else(
        |_| "unknown".to_owned(),
        |c| enum_name(c.as_str_name(), "MIRROR_ERROR_CODE_"),
    );
    json!({
        "last_poll_at_ms": s.last_poll_at_ms,
        "last_success_at_ms": s.last_success_at_ms,
        "last_imported_sha": s.last_imported_sha,
        "last_changeset": s.last_changeset.as_deref().map(id_text),
        "lag_changesets": s.lag_changesets,
        "lag_seconds": s.lag_seconds,
        "error_code": if code == "unspecified" { Value::Null } else { Value::String(code) },
        "error_message": s.error_message,
        "backoff_until_ms": s.backoff_until_ms,
        "lease_epoch": s.lease_epoch,
    })
}

/// The JSON form of a batch, rewind or purge request.
#[must_use]
pub fn request_json(r: &pb::MirrorRequest) -> Value {
    let kind = pb::MirrorRequestKind::try_from(r.kind).map_or_else(
        |_| "unknown".to_owned(),
        |k| enum_name(k.as_str_name(), "MIRROR_REQUEST_KIND_"),
    );
    let state = pb::MirrorRequestState::try_from(r.state).map_or_else(
        |_| "unknown".to_owned(),
        |s| enum_name(s.as_str_name(), "MIRROR_REQUEST_STATE_"),
    );
    let batch_state = pb::BatchState::try_from(r.batch_state).map_or_else(
        |_| "unknown".to_owned(),
        |s| enum_name(s.as_str_name(), "BATCH_STATE_"),
    );
    let result = r.result.clone().unwrap_or_default();
    json!({
        "repo": r.repo,
        "id": r.id,
        "line": r.line,
        "kind": kind,
        "client_request_id": r.client_request_id,
        "base": r.base.as_deref().map(id_text),
        "excludes": r.excludes.iter().map(|e| id_text(e)).collect::<Vec<_>>(),
        "upstream": r.upstream,
        "state": state,
        "batch_state": if batch_state == "unspecified" { Value::Null } else { Value::String(batch_state) },
        "claimed_by_epoch": r.claimed_by_epoch,
        "claimed_at_ms": r.claimed_at_ms,
        "deadline_ms": r.deadline_ms,
        "result": {
            "pr_number": result.pr_number,
            "pr_url": result.pr_url,
            "branch": result.branch,
            "lands": result.lands.iter().map(|l| id_text(l)).collect::<Vec<_>>(),
            "error_code": result.error_code,
        },
        "requested_by": r.requested_by,
        "created_at_ms": r.created_at_ms,
        "updated_at_ms": r.updated_at_ms,
    })
}

/// Parses `branch=line` pairs, each with `direction`.
pub fn parse_lines(pairs: &[String], direction: &str) -> Result<Vec<pb::MirrorLine>, String> {
    pairs
        .iter()
        .map(|pair| match pair.split_once('=') {
            Some((branch, line)) if !branch.is_empty() && !line.is_empty() => Ok(pb::MirrorLine {
                branch: branch.to_owned(),
                line: line.to_owned(),
                direction: direction.to_owned(),
            }),
            _ => Err(format!("`{pair}` is not BRANCH=LINE")),
        })
        .collect()
}

/// Joins follow and export pairs: a pair named by both is `both`.
pub fn join_lines(follow: Vec<pb::MirrorLine>, export: Vec<pb::MirrorLine>) -> Vec<pb::MirrorLine> {
    let mut out = follow;
    for e in export {
        match out
            .iter_mut()
            .find(|l| l.branch == e.branch && l.line == e.line)
        {
            Some(l) => l.direction = "both".to_owned(),
            None => out.push(e),
        }
    }
    out
}

/// Longest repository name or upstream URL the tools pass on; the server
/// applies its own rules after this.
const ARG_MAX_BYTES: usize = 2_048;
/// Default and largest page a list tool asks for.
const PAGE_DEFAULT: u32 = 20;
const PAGE_MAX: u32 = 100;

fn text_arg(field: &str, value: &str) -> Result<(), ToolError> {
    if value.is_empty() || value.len() > ARG_MAX_BYTES || value.chars().any(char::is_control) {
        return Err(ToolError::Invalid(format!(
            "`{field}` must be short, printable and non-empty"
        )));
    }
    Ok(())
}

fn page(limit: Option<u32>) -> Result<u32, ToolError> {
    match limit.unwrap_or(PAGE_DEFAULT) {
        n if (1..=PAGE_MAX).contains(&n) => Ok(n),
        n => Err(ToolError::Invalid(format!(
            "limit {n} is outside 1 to {PAGE_MAX}"
        ))),
    }
}

fn request_id(given: Option<String>) -> Result<String, ToolError> {
    match given {
        Some(id) => {
            text_arg("client_request_id", &id)?;
            Ok(id)
        }
        None => Ok(new_client_request_id("mcp")),
    }
}

/// One mirror line, as a tool takes it.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MirrorLineArg {
    /// The upstream branch.
    pub branch: String,
    /// The graft line.
    pub line: String,
    /// `follow` (the default), `export` or `both`.
    #[serde(default)]
    pub direction: Option<String>,
}

/// Arguments of `graft_mirror_add`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MirrorAddArgs {
    /// The graft repository.
    pub repo: String,
    /// The upstream URL (`https://…` or `ssh://…`), on the server's allowlist.
    pub upstream: String,
    /// The branches to mirror and their lines.
    pub lines: Vec<MirrorLineArg>,
    /// Poll interval in seconds; the server's default when absent.
    #[serde(default)]
    pub interval_s: Option<u32>,
    /// Start disabled.
    #[serde(default)]
    pub disabled: bool,
}

/// Arguments of `graft_mirror_set`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MirrorSetArgs {
    /// The graft repository.
    pub repo: String,
    /// The mirror's upstream URL.
    pub upstream: String,
    /// Enable or disable the mirror.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Replace the lines (changing an export line needs mirror:export).
    #[serde(default)]
    pub lines: Option<Vec<MirrorLineArg>>,
    /// The new poll interval in seconds.
    #[serde(default)]
    pub interval_s: Option<u32>,
}

/// Arguments that name one mirror.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MirrorKeyArgs {
    /// The graft repository.
    pub repo: String,
    /// The mirror's upstream URL.
    pub upstream: String,
}

/// Arguments of `graft_mirror_remove`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MirrorRemoveArgs {
    /// The graft repository.
    pub repo: String,
    /// The mirror's upstream URL.
    pub upstream: String,
    /// Also clear the followed-line markers, opening the lines to lands.
    #[serde(default)]
    pub clear_followed: bool,
}

/// Arguments of `graft_mirror_purge`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MirrorPurgeArgs {
    /// The graft repository.
    pub repo: String,
    /// The mirror's upstream URL.
    pub upstream: String,
    /// Idempotency key; pass the one a failed call returned to retry it.
    #[serde(default)]
    pub client_request_id: Option<String>,
}

/// Arguments of `graft_mirror_list`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MirrorListArgs {
    /// The repository, or `*` for every repository you can read.
    pub repo: String,
    /// Only enabled (true) or disabled (false) mirrors.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Page size, 1 to 100 (default 20).
    #[serde(default)]
    pub limit: Option<u32>,
    /// The `next_page_token` of the previous page.
    #[serde(default)]
    pub page_token: Option<String>,
}

/// Arguments of `graft_batch_cut`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchCutArgs {
    /// The graft repository.
    pub repo: String,
    /// The line to batch.
    pub line: String,
    /// The changeset the batch starts after.
    #[serde(default)]
    pub base: Option<String>,
    /// Changesets to leave out.
    #[serde(default)]
    pub excludes: Vec<String>,
    /// Idempotency key; pass the one a failed call returned to retry it.
    #[serde(default)]
    pub client_request_id: Option<String>,
}

/// Arguments of `graft_batch_status`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchStatusArgs {
    /// The graft repository.
    pub repo: String,
    /// The line.
    pub line: String,
}

/// Arguments of `graft_batch_list`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchListArgs {
    /// The graft repository.
    pub repo: String,
    /// Only this line's batches.
    #[serde(default)]
    pub line: Option<String>,
    /// Page size, 1 to 100 (default 20).
    #[serde(default)]
    pub limit: Option<u32>,
    /// The `next_page_token` of the previous page.
    #[serde(default)]
    pub page_token: Option<String>,
}

fn line_args(lines: Vec<MirrorLineArg>) -> Result<Vec<pb::MirrorLine>, ToolError> {
    lines
        .into_iter()
        .map(|l| {
            text_arg("branch", &l.branch)?;
            text_arg("line", &l.line)?;
            Ok(pb::MirrorLine {
                branch: l.branch,
                line: l.line,
                direction: l.direction.unwrap_or_else(|| "follow".to_owned()),
            })
        })
        .collect()
}

impl<A: RepoApi + WorkspaceApi + Send + Sync + 'static> McpServer<A> {
    /// Serves the `graft_mirror_*` and `graft_batch_*` tools through `admin`.
    #[must_use]
    pub fn with_mirror_admin(mut self, admin: Arc<dyn MirrorAdmin>) -> Self {
        self.mirror_admin = Some(admin);
        self
    }

    fn mirrors(&self, tool: &str) -> Result<&Arc<dyn MirrorAdmin>, ToolError> {
        self.mirror_admin
            .as_ref()
            .ok_or_else(|| ToolError::Invalid(format!("{tool} is not available on this server")))
    }

    async fn mirror_add(
        &self,
        cx: &CallContext,
        a: MirrorAddArgs,
    ) -> Result<CallToolResult, ToolError> {
        text_arg("repo", &a.repo)?;
        text_arg("upstream", &a.upstream)?;
        let admin = self.mirrors("graft_mirror_add")?;
        let req = pb::SetMirrorRequest {
            repo: a.repo,
            upstream: a.upstream,
            lines: line_args(a.lines)?,
            interval_s: a.interval_s.unwrap_or(0),
            enabled: !a.disabled,
            export: None,
            expected_version: None,
        };
        let row = admin.set_mirror(cx, req).await.map_err(ToolError::Api)?;
        bounded(json!({ "mirror": mirror_json(&row) }))
    }

    async fn mirror_set(
        &self,
        cx: &CallContext,
        a: MirrorSetArgs,
    ) -> Result<CallToolResult, ToolError> {
        text_arg("repo", &a.repo)?;
        text_arg("upstream", &a.upstream)?;
        let admin = self.mirrors("graft_mirror_set")?;
        let key = pb::MirrorKey {
            repo: a.repo.clone(),
            upstream: a.upstream.clone(),
        };
        let old = admin.get_mirror(cx, key).await.map_err(ToolError::Api)?;
        let req = pb::SetMirrorRequest {
            repo: a.repo,
            upstream: a.upstream,
            lines: match a.lines {
                Some(lines) => line_args(lines)?,
                None => old.lines,
            },
            interval_s: a.interval_s.unwrap_or(old.interval_s),
            enabled: a.enabled.unwrap_or(old.enabled),
            export: old.export,
            expected_version: Some(old.version),
        };
        let row = admin.set_mirror(cx, req).await.map_err(ToolError::Api)?;
        bounded(json!({ "mirror": mirror_json(&row) }))
    }

    async fn mirror_status(
        &self,
        cx: &CallContext,
        a: MirrorKeyArgs,
    ) -> Result<CallToolResult, ToolError> {
        text_arg("repo", &a.repo)?;
        text_arg("upstream", &a.upstream)?;
        let admin = self.mirrors("graft_mirror_status")?;
        let key = pb::MirrorKey {
            repo: a.repo,
            upstream: a.upstream,
        };
        let row = admin.get_mirror(cx, key).await.map_err(ToolError::Api)?;
        bounded(json!({ "mirror": mirror_json(&row) }))
    }

    async fn mirror_list(
        &self,
        cx: &CallContext,
        a: MirrorListArgs,
    ) -> Result<CallToolResult, ToolError> {
        text_arg("repo", &a.repo)?;
        let admin = self.mirrors("graft_mirror_list")?;
        let size = page(a.limit)?;
        let repo = if a.repo == "*" { String::new() } else { a.repo };
        let token = a.page_token.unwrap_or_default();
        fit(
            size,
            |limit| {
                admin.list_mirrors(
                    cx,
                    pb::ListMirrorsRequest {
                        repo: repo.clone(),
                        enabled: a.enabled,
                        limit,
                        page_token: token.clone(),
                    },
                )
            },
            |p: pb::MirrorPage, _| {
                json!({
                    "mirrors": p.mirrors.iter().map(mirror_json).collect::<Vec<_>>(),
                    "next_page_token": (!p.next_page_token.is_empty()).then_some(p.next_page_token),
                })
            },
        )
        .await
    }

    async fn mirror_remove(
        &self,
        cx: &CallContext,
        a: MirrorRemoveArgs,
    ) -> Result<CallToolResult, ToolError> {
        text_arg("repo", &a.repo)?;
        text_arg("upstream", &a.upstream)?;
        let admin = self.mirrors("graft_mirror_remove")?;
        let req = pb::RemoveMirrorRequest {
            repo: a.repo.clone(),
            upstream: a.upstream.clone(),
            expected_version: None,
            clear_followed: a.clear_followed,
        };
        let out = admin.remove_mirror(cx, req).await.map_err(ToolError::Api)?;
        bounded(json!({
            "repo": a.repo,
            "upstream": a.upstream,
            "cleared": out.cleared,
        }))
    }

    async fn mirror_purge(
        &self,
        cx: &CallContext,
        a: MirrorPurgeArgs,
    ) -> Result<CallToolResult, ToolError> {
        text_arg("repo", &a.repo)?;
        text_arg("upstream", &a.upstream)?;
        let admin = self.mirrors("graft_mirror_purge")?;
        let req = pb::PurgeMirrorRequest {
            repo: a.repo,
            upstream: a.upstream,
            client_request_id: request_id(a.client_request_id)?,
        };
        let done = with_retry(|| admin.purge_mirror(cx, req.clone())).await;
        let r = done.map_err(|e| retry_hint(e, &req.client_request_id))?;
        bounded(json!({ "request": request_json(&r) }))
    }

    async fn batch_cut(
        &self,
        cx: &CallContext,
        a: BatchCutArgs,
    ) -> Result<CallToolResult, ToolError> {
        text_arg("repo", &a.repo)?;
        text_arg("line", &a.line)?;
        let admin = self.mirrors("graft_batch_cut")?;
        let base = a
            .base
            .as_deref()
            .map(|b| changeset_bytes("base", b))
            .transpose()
            .map_err(ToolError::Invalid)?;
        let excludes = a
            .excludes
            .iter()
            .map(|e| changeset_bytes("exclude", e))
            .collect::<Result<Vec<_>, _>>()
            .map_err(ToolError::Invalid)?;
        let req = pb::CutBatchRequest {
            repo: a.repo,
            line: a.line,
            base,
            excludes,
            client_request_id: request_id(a.client_request_id)?,
        };
        let done = with_retry(|| admin.cut_batch(cx, req.clone())).await;
        let r = done.map_err(|e| retry_hint(e, &req.client_request_id))?;
        bounded(json!({ "request": request_json(&r) }))
    }

    async fn batch_status(
        &self,
        cx: &CallContext,
        a: BatchStatusArgs,
    ) -> Result<CallToolResult, ToolError> {
        text_arg("repo", &a.repo)?;
        text_arg("line", &a.line)?;
        let admin = self.mirrors("graft_batch_status")?;
        let key = pb::BatchKey {
            repo: a.repo,
            line: a.line,
        };
        let view = admin.get_batch(cx, key).await.map_err(ToolError::Api)?;
        bounded(json!({ "batch": view.batch.as_ref().map(request_json) }))
    }

    async fn batch_list(
        &self,
        cx: &CallContext,
        a: BatchListArgs,
    ) -> Result<CallToolResult, ToolError> {
        text_arg("repo", &a.repo)?;
        let admin = self.mirrors("graft_batch_list")?;
        let size = page(a.limit)?;
        let token = a.page_token.unwrap_or_default();
        fit(
            size,
            |limit| {
                admin.list_batches(
                    cx,
                    pb::ListBatchesRequest {
                        repo: a.repo.clone(),
                        line: a.line.clone(),
                        page_token: token.clone(),
                        limit,
                    },
                )
            },
            |p: pb::BatchPage, _| {
                json!({
                    "batches": p.batches.iter().map(request_json).collect::<Vec<_>>(),
                    "next_page_token": (!p.next_page_token.is_empty()).then_some(p.next_page_token),
                })
            },
        )
        .await
    }
}

/// A request that still failed after its retries: a retryable error names
/// the key, so the caller's own retry returns the same request.
fn retry_hint(e: ApiError, client_request_id: &str) -> ToolError {
    if e.is_retryable() {
        ToolError::Local {
            kind: "retry_with_same_request_id",
            message: format!(
                "the server did not answer; call again with client_request_id `{client_request_id}`"
            ),
        }
    } else {
        ToolError::Api(e)
    }
}

#[tool_router(router = mirrors_tool_router, vis = "pub(crate)")]
impl<A: RepoApi + WorkspaceApi + Send + Sync + 'static> McpServer<A> {
    #[tool(
        name = "graft_mirror_add",
        description = "Bind a repository to an upstream git URL and its branches. Needs mirror:bind, which only an operator holds; other callers get permission_denied.",
        annotations(
            title = "Add mirror",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn graft_mirror_add(
        &self,
        Parameters(args): Parameters<MirrorAddArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.mirror_add(&cx, args).await
        })
        .await)
    }

    #[tool(
        name = "graft_mirror_set",
        description = "Enable or disable a mirror, or change its lines or poll interval. Needs the repository's admin; changing an export line needs mirror:export.",
        annotations(
            title = "Change mirror",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn graft_mirror_set(
        &self,
        Parameters(args): Parameters<MirrorSetArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.mirror_set(&cx, args).await
        })
        .await)
    }

    #[tool(
        name = "graft_mirror_status",
        description = "One mirror's settings and last poll status. Says whether a credential is set, never the credential.",
        annotations(
            title = "Mirror status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn graft_mirror_status(
        &self,
        Parameters(args): Parameters<MirrorKeyArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.mirror_status(&cx, args).await
        })
        .await)
    }

    #[tool(
        name = "graft_mirror_list",
        description = "List mirrors of a repository, or of every repository you can read with repo `*`; paginated.",
        annotations(
            title = "List mirrors",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn graft_mirror_list(
        &self,
        Parameters(args): Parameters<MirrorListArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.mirror_list(&cx, args).await
        })
        .await)
    }

    #[tool(
        name = "graft_mirror_remove",
        description = "Remove a mirror. Its lines stay closed to lands unless clear_followed is set. Needs mirror:bind (operator only).",
        annotations(
            title = "Remove mirror",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn graft_mirror_remove(
        &self,
        Parameters(args): Parameters<MirrorRemoveArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.mirror_remove(&cx, args).await
        })
        .await)
    }

    #[tool(
        name = "graft_mirror_purge",
        description = "Request deletion of a removed or disabled mirror's local data. The graft repository is untouched. Needs mirror:bind (operator only).",
        annotations(
            title = "Purge mirror data",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn graft_mirror_purge(
        &self,
        Parameters(args): Parameters<MirrorPurgeArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.mirror_purge(&cx, args).await
        })
        .await)
    }

    #[tool(
        name = "graft_batch_cut",
        description = "Request a batch PR of a line's lands. A retry with the same client_request_id returns the same request. Needs the repository's admin.",
        annotations(
            title = "Cut batch",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn graft_batch_cut(
        &self,
        Parameters(args): Parameters<BatchCutArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.batch_cut(&cx, args).await
        })
        .await)
    }

    #[tool(
        name = "graft_batch_status",
        description = "The open batch of a line: its request, PR and state.",
        annotations(
            title = "Batch status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn graft_batch_status(
        &self,
        Parameters(args): Parameters<BatchStatusArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.batch_status(&cx, args).await
        })
        .await)
    }

    #[tool(
        name = "graft_batch_list",
        description = "A repository's batch history, optionally of one line; paginated.",
        annotations(
            title = "List batches",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn graft_batch_list(
        &self,
        Parameters(args): Parameters<BatchListArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(respond(async {
            let cx = self.call_context(&ctx)?;
            self.batch_list(&cx, args).await
        })
        .await)
    }
}
