//! Plain HTTP beside gRPC: `/healthz`, `/readyz`, `/metrics` and the
//! read-only JSON facade under `/v1` (`docs/design.md` §7.1, §13).
//!
//! The facade is for scripts and simple tools. It takes the same bearer
//! token as gRPC and calls the same engine operations, so scopes and limits
//! are identical. Ids use their text form (`c…`, `t…`, `b…`); wherever a
//! changeset is expected (`{at}`, `from`, `to`) a ref name is accepted too.
//!
//! | Route | Operation |
//! | --- | --- |
//! | `GET /v1/repos/{repo}/refs?prefix=&limit=&page_token=` | `ListRefs` |
//! | `GET /v1/repos/{repo}/refs/{name}` | `ResolveRef` |
//! | `GET /v1/repos/{repo}/changesets/{id}` | `GetChangesets` |
//! | `GET /v1/repos/{repo}/stat/{at}?path=` | `Stat` |
//! | `GET /v1/repos/{repo}/tree/{at}?path=&recursive=&limit=&page_token=` | `ListTree` |
//! | `GET /v1/repos/{repo}/file/{at}?path=` | `ReadFile` (raw bytes) |
//! | `GET /v1/repos/{repo}/diff?from=&to=&path=&patch=&context=` | `Diff` |
//! | `GET /v1/repos/{repo}/journal?from_lsn=&limit=` | `Watch` (one page) |
//! | `GET /v1/repos/{repo}/log?from=&until=&path=&limit=&page_token=` | `Log` |
//! | `GET /v1/repos/{repo}/blame/{at}?path=` | `Blame` |
//! | `GET /v1/repos/{repo}/drafts/overlap?line=&path=&include_own=&limit=&page_token=` | `OverlappingDrafts` |
//! | `GET /v1/repos/{repo}/attestations?changeset=` or `line=&from_pos=&to_pos=` `&kind=&limit=&page_token=` | `GetAttestations` |
//! | `GET /v1/repos/{repo}/land-checks?line=&from_pos=&limit=&page_token=` | `GetLandChecks` |
//! | `GET /v1/repos/{repo}/review-queue?line=&since_lsn=&path=&max_lines=&limit=` | `ReviewQueue` |
//!
//! `log` and `drafts/overlap` page by `page_token`: pass the previous answer's `next`.
//! Review reads page the same way, except `review-queue`, which resumes with `next_lsn`
//! as the next `since_lsn` and rejects `page_token`. A `limit` above the review cap is
//! clamped. Nothing here gates a land.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use graft_model::{AuthorKind, Change, ChangesetId, EntryKind, ObjectId, RepoPath, Timestamp};
use graft_proto::types::*;
use graft_proto::{ApiError, RepoApi as _, WorkspaceApi as _};
use serde_json::{Value, json};

use crate::app::{App, CallError, Caller, error_name};

/// The health and metrics routes, served on every listener.
pub(crate) fn ops_router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .with_state(app)
}

/// The JSON facade.
pub(crate) fn facade_router(app: Arc<App>) -> Router {
    Router::new()
        .route("/v1/repos/{repo}/refs", get(list_refs))
        .route("/v1/repos/{repo}/refs/{*name}", get(resolve_ref))
        .route("/v1/repos/{repo}/changesets/{id}", get(changeset))
        .route("/v1/repos/{repo}/stat/{at}", get(stat))
        .route("/v1/repos/{repo}/tree/{at}", get(list_tree))
        .route("/v1/repos/{repo}/file/{at}", get(read_file))
        .route("/v1/repos/{repo}/diff", get(diff))
        .route("/v1/repos/{repo}/journal", get(journal))
        .route("/v1/repos/{repo}/log", get(log))
        .route("/v1/repos/{repo}/blame/{at}", get(blame))
        .route("/v1/repos/{repo}/drafts/overlap", get(draft_overlap))
        .route("/v1/repos/{repo}/attestations", get(attestations))
        .route("/v1/repos/{repo}/land-checks", get(land_checks))
        .route("/v1/repos/{repo}/review-queue", get(review_queue))
        .with_state(app)
}

async fn healthz() -> &'static str {
    "ok\n"
}

async fn readyz(State(app): State<Arc<App>>) -> Response {
    if app.is_ready() {
        (StatusCode::OK, "ready\n").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready\n").into_response()
    }
}

async fn metrics(State(app): State<Arc<App>>) -> Response {
    app.admission.sample(app.telemetry.metrics());
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4"),
        )],
        app.telemetry.encode(),
    )
        .into_response()
}

// ---- errors ---------------------------------------------------------------

impl IntoResponse for CallError {
    fn into_response(self) -> Response {
        match self {
            CallError::Api(e) => api_error(&e),
            CallError::Unimplemented(op) => error_body(
                StatusCode::NOT_IMPLEMENTED,
                "unimplemented",
                &format!("{op} is not implemented in this release"),
            ),
            CallError::Internal => error_body(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "internal error",
            ),
        }
    }
}

fn api_error(e: &ApiError) -> Response {
    use tonic::Code;
    let status = match e.grpc_code() {
        Code::NotFound => StatusCode::NOT_FOUND,
        Code::InvalidArgument => StatusCode::BAD_REQUEST,
        Code::FailedPrecondition => StatusCode::PRECONDITION_FAILED,
        Code::Aborted => StatusCode::CONFLICT,
        Code::PermissionDenied => StatusCode::FORBIDDEN,
        Code::ResourceExhausted => StatusCode::PAYLOAD_TOO_LARGE,
        Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        Code::Unauthenticated => StatusCode::UNAUTHORIZED,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let mut response = error_body(status, error_name(e), &e.to_string());
    let headers = response.headers_mut();
    match e {
        ApiError::Unauthenticated => {
            headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        ApiError::Unavailable { retry_after_ms } => {
            let secs = retry_after_ms.div_ceil(1000).max(1);
            if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                headers.insert(header::RETRY_AFTER, v);
            }
        }
        _ => {}
    }
    response
}

fn error_body(status: StatusCode, kind: &str, message: &str) -> Response {
    (
        status,
        axum::Json(json!({ "error": { "kind": kind, "message": message } })),
    )
        .into_response()
}

fn bad(field: &str, reason: &str) -> CallError {
    CallError::Api(ApiError::InvalidChange {
        path: String::new(),
        rule: format!("{field}: {reason}"),
    })
}

type Answer = Result<Response, CallError>;

// ---- query strings --------------------------------------------------------

/// Decoded `key=value` pairs, in order (repeated keys kept).
struct Query(Vec<(String, String)>);

impl Query {
    fn parse(raw: Option<String>) -> Result<Self, CallError> {
        let mut pairs = Vec::new();
        for part in raw
            .as_deref()
            .unwrap_or("")
            .split('&')
            .filter(|p| !p.is_empty())
        {
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            pairs.push((decode(k)?, decode(v)?));
        }
        Ok(Self(pairs))
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn all(&self, key: &str) -> impl Iterator<Item = &str> {
        self.0
            .iter()
            .filter(move |(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn num<T: std::str::FromStr>(&self, key: &str) -> Result<Option<T>, CallError> {
        self.get(key)
            .map(|v| v.parse().map_err(|_| bad(key, "not a number")))
            .transpose()
    }

    fn flag(&self, key: &str) -> Result<bool, CallError> {
        match self.get(key) {
            None | Some("false" | "0") => Ok(false),
            Some("" | "true" | "1") => Ok(true),
            Some(_) => Err(bad(key, "not a boolean")),
        }
    }

    fn page(&self) -> Result<Page, CallError> {
        Ok(Page {
            limit: self.num("limit")?.unwrap_or(0),
            token: self.get("page_token").map(str::to_owned),
        })
    }

    fn path(&self) -> Result<Option<RepoPath>, CallError> {
        self.get("path")
            .filter(|p| !p.is_empty())
            .map(repo_path)
            .transpose()
    }

    fn paths(&self) -> Result<Vec<RepoPath>, CallError> {
        self.all("path")
            .filter(|p| !p.is_empty())
            .map(repo_path)
            .collect()
    }
}

/// Percent-decoding, with `+` as a space.
fn decode(s: &str) -> Result<String, CallError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = bytes
                    .get(i + 1..i + 3)
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or_else(|| bad("query", "bad percent escape"))?;
                out.push(hex);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).map_err(|_| bad("query", "not UTF-8"))
}

fn repo_path(p: &str) -> Result<RepoPath, CallError> {
    RepoPath::new(p).map_err(|e| {
        CallError::Api(ApiError::InvalidPath {
            path: p.to_owned(),
            reason: e.reason().to_owned(),
        })
    })
}

// ---- calls ----------------------------------------------------------------

fn cx(app: &App, headers: &HeaderMap, op: &'static str) -> Result<Caller, CallError> {
    app.authenticate(|k| headers.get(k).and_then(|v| v.to_str().ok()), false)
        .map_err(|e| {
            app.refused(op, &e);
            CallError::Api(e)
        })
}

/// A changeset id in text form, or else a ref name resolved now.
async fn at(app: &App, cx: &Caller, repo: &str, text: &str) -> Result<ChangesetId, CallError> {
    if let Ok(id) = ChangesetId::parse(text) {
        return Ok(id);
    }
    let req = ResolveRef {
        repo: repo.to_owned(),
        name: text.to_owned(),
    };
    let view = app
        .call(
            "ResolveRef",
            repo,
            cx.clone(),
            false,
            move |e, cx| async move { e.resolve_ref(&cx, req).await },
        )
        .await?;
    Ok(view.target)
}

fn ok(v: Value) -> Answer {
    Ok(axum::Json(v).into_response())
}

async fn list_refs(
    State(app): State<Arc<App>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Answer {
    let cx = cx(&app, &headers, "ListRefs")?;
    let q = Query::parse(q)?;
    let req = ListRefs {
        repo: repo.clone(),
        prefix: q.get("prefix").map(str::to_owned),
        page: q.page()?,
    };
    let refs = app
        .call("ListRefs", &repo, cx, false, move |e, cx| async move {
            e.list_refs(&cx, req).await
        })
        .await?;
    let next = refs.last().map(|r| r.name.clone());
    ok(json!({ "refs": refs.iter().map(ref_json).collect::<Vec<_>>(), "next": next }))
}

async fn resolve_ref(
    State(app): State<Arc<App>>,
    Path((repo, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Answer {
    let cx = cx(&app, &headers, "ResolveRef")?;
    let req = ResolveRef {
        repo: repo.clone(),
        name,
    };
    let view = app
        .call("ResolveRef", &repo, cx, false, move |e, cx| async move {
            e.resolve_ref(&cx, req).await
        })
        .await?;
    ok(ref_json(&view))
}

async fn changeset(
    State(app): State<Arc<App>>,
    Path((repo, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Answer {
    let cx = cx(&app, &headers, "GetChangesets")?;
    let id = at(&app, &cx, &repo, &id).await?;
    let req = GetChangesets {
        repo: repo.clone(),
        ids: vec![id],
    };
    let mut found = app
        .call("GetChangesets", &repo, cx, false, move |e, cx| async move {
            e.get_changesets(&cx, req).await
        })
        .await?;
    if found.collected.contains(&id) {
        return Err(CallError::Api(ApiError::Collected { id }));
    }
    let view = found.found.pop().ok_or_else(|| {
        CallError::Api(ApiError::NotFound {
            what: format!("changeset {id}"),
        })
    })?;
    let cs = &view.changeset;
    ok(json!({
        "id": id.to_string(),
        "parents": cs.parents.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "root": cs.root.to_string(),
        "author": { "name": cs.author.name, "email": cs.author.email, "kind": author_kind(cs.author.kind) },
        "authored_at": time_json(cs.authored_at),
        "message": cs.message,
        "metadata": cs.metadata,
        "changes": cs.changes.iter().map(change_json).collect::<Vec<_>>(),
        "lsn": view.lsn,
        "state": draft_state(view.state),
        "superseded_by": view.superseded_by.map(|s| s.to_string()),
    }))
}

async fn stat(
    State(app): State<Arc<App>>,
    Path((repo, at_text)): Path<(String, String)>,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Answer {
    let cx = cx(&app, &headers, "Stat")?;
    let q = Query::parse(q)?;
    let path = q.path()?.ok_or_else(|| bad("path", "required"))?;
    let at = at(&app, &cx, &repo, &at_text).await?;
    let req = PathAt {
        repo: repo.clone(),
        at,
        path,
    };
    let entry = app
        .call("Stat", &repo, cx, false, move |e, cx| async move {
            e.stat(&cx, req).await
        })
        .await?;
    ok(entry_json(&entry))
}

async fn list_tree(
    State(app): State<Arc<App>>,
    Path((repo, at_text)): Path<(String, String)>,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Answer {
    let cx = cx(&app, &headers, "ListTree")?;
    let q = Query::parse(q)?;
    let at = at(&app, &cx, &repo, &at_text).await?;
    let req = ListTree {
        repo: repo.clone(),
        at,
        path: q.path()?,
        recursive: q.flag("recursive")?,
        page: q.page()?,
    };
    let entries = app
        .call("ListTree", &repo, cx, false, move |e, cx| async move {
            e.list_tree(&cx, req).await
        })
        .await?;
    ok(json!({
        "entries": entries.entries.iter().map(entry_json).collect::<Vec<_>>(),
        "next": entries.next,
    }))
}

async fn read_file(
    State(app): State<Arc<App>>,
    Path((repo, at_text)): Path<(String, String)>,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Answer {
    let cx = cx(&app, &headers, "ReadFile")?;
    let q = Query::parse(q)?;
    let path = q.path()?.ok_or_else(|| bad("path", "required"))?;
    let at = at(&app, &cx, &repo, &at_text).await?;
    let req = PathAt {
        repo: repo.clone(),
        at,
        path,
    };
    let file = app
        .call("ReadFile", &repo, cx, false, move |e, cx| async move {
            e.read_file(&cx, req).await
        })
        .await?;
    let mut response = file.data.into_response();
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(v) = HeaderValue::from_str(&file.id.to_string()) {
        h.insert("graft-id", v);
    }
    h.insert(
        "graft-kind",
        HeaderValue::from_static(entry_kind(file.kind)),
    );
    Ok(response)
}

async fn diff(
    State(app): State<Arc<App>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Answer {
    let cx = cx(&app, &headers, "Diff")?;
    let q = Query::parse(q)?;
    let from = q.get("from").ok_or_else(|| bad("from", "required"))?;
    let to = q.get("to").ok_or_else(|| bad("to", "required"))?;
    let from = at(&app, &cx, &repo, from).await?;
    let to = at(&app, &cx, &repo, to).await?;
    let patch = if q.flag("patch")? {
        Some(PatchOptions {
            context: q.num("context")?.unwrap_or(3),
            max_bytes_per_file: q.num("max_bytes_per_file")?.unwrap_or(0),
        })
    } else {
        None
    };
    let req = Diff {
        repo: repo.clone(),
        from,
        to,
        paths: q.paths()?,
        patch,
        page: q.page()?,
    };
    let result = app
        .call("Diff", &repo, cx, false, move |e, cx| async move {
            e.diff(&cx, req).await
        })
        .await?;
    let files: Vec<Value> = result
        .files
        .iter()
        .map(|f| {
            let mut v = change_json(&f.change);
            if let Some(patch) = &f.patch {
                v["patch"] = json!(patch);
                v["truncated"] = json!(f.truncated);
            }
            v
        })
        .collect();
    ok(json!({ "files": files, "next": result.next }))
}

async fn journal(
    State(app): State<Arc<App>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Answer {
    let cx = cx(&app, &headers, "Watch")?;
    let q = Query::parse(q)?;
    let req = Watch {
        repo: repo.clone(),
        from_lsn: q.num("from_lsn")?.unwrap_or(1),
        kinds: q.all("kind").map(str::to_owned).collect(),
        limit: q.num("limit")?.unwrap_or(0),
        wait_ms: 0,
    };
    let page = app
        .call("Watch", &repo, cx, false, move |e, cx| async move {
            e.watch(&cx, req).await
        })
        .await?;
    let entries: Vec<Value> = page
        .entries
        .iter()
        .map(|e| {
            // Bodies are JSON written by the engine; pass them through.
            let body = serde_json::from_str::<Value>(&e.body).unwrap_or(json!(e.body));
            json!({ "lsn": e.lsn, "kind": e.kind, "body": body })
        })
        .collect();
    ok(json!({ "entries": entries, "next_lsn": page.next_lsn }))
}

async fn log(
    State(app): State<Arc<App>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Answer {
    let cx = cx(&app, &headers, "Log")?;
    let q = Query::parse(q)?;
    let from = q.get("from").ok_or_else(|| bad("from", "required"))?;
    let from = at(&app, &cx, &repo, from).await?;
    let until = match q.get("until") {
        Some(u) => Some(at(&app, &cx, &repo, u).await?),
        None => None,
    };
    let req = Log {
        repo: repo.clone(),
        from,
        until,
        paths: q.paths()?,
        page: q.page()?,
    };
    let history = app
        .call("Log", &repo, cx, false, move |e, cx| async move {
            e.log(&cx, req).await
        })
        .await?;
    ok(json!({
        "changesets": history.changesets.iter().map(summary_json).collect::<Vec<_>>(),
        "next": history.next,
    }))
}

async fn draft_overlap(
    State(app): State<Arc<App>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Answer {
    let cx = cx(&app, &headers, "OverlappingDrafts")?;
    let q = Query::parse(q)?;
    let line = q.get("line").ok_or_else(|| bad("line", "required"))?;
    let req = OverlappingDrafts {
        repo: repo.clone(),
        line: line.to_owned(),
        paths: q.paths()?,
        include_own: q.flag("include_own")?,
        page: q.page()?,
    };
    let overlap = app
        .call(
            "OverlappingDrafts",
            &repo,
            cx,
            false,
            move |e, cx| async move { e.overlapping_drafts(&cx, req).await },
        )
        .await?;
    ok(draft_overlap_json(&overlap))
}

/// An overlap page as JSON: paths, workspace names and metadata only.
pub(crate) fn draft_overlap_json(o: &DraftOverlap) -> Value {
    json!({
        "hits": o.hits.iter().map(|h| json!({
            "path": h.path.as_str(),
            "workspace": h.workspace,
        })).collect::<Vec<_>>(),
        "workspaces": o.workspaces.iter().map(|w| json!({
            "workspace": w.workspace,
            "owner": w.owner,
            "session": w.session,
            "work_item": w.work_item,
            "draft_age_ms": w.draft_age_ms,
            "lease_until_ms": w.lease_until_ms,
        })).collect::<Vec<_>>(),
        "next": o.next,
    })
}

async fn blame(
    State(app): State<Arc<App>>,
    Path((repo, at_text)): Path<(String, String)>,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Answer {
    let cx = cx(&app, &headers, "Blame")?;
    let q = Query::parse(q)?;
    let path = q.path()?.ok_or_else(|| bad("path", "required"))?;
    let at = at(&app, &cx, &repo, &at_text).await?;
    let req = Blame {
        repo: repo.clone(),
        at,
        path,
        lines: None,
    };
    let result = app
        .call("Blame", &repo, cx, false, move |e, cx| async move {
            e.blame(&cx, req).await
        })
        .await?;
    let ranges: Vec<Value> = result
        .ranges
        .iter()
        .map(|r| json!({ "start": r.start, "end": r.end, "origin": summary_json(&r.origin) }))
        .collect();
    ok(json!({ "ranges": ranges }))
}

// ---- JSON shapes ----------------------------------------------------------

fn ref_json(r: &RefView) -> Value {
    let kind = match r.kind {
        RefKind::Line => "line",
        RefKind::Tag => "tag",
        RefKind::Tool => "tool",
    };
    json!({ "name": r.name, "target": r.target.to_string(), "version": r.version, "kind": kind })
}

/// An entry's id in the text form of what it names.
fn entry_id(kind: EntryKind, id: ObjectId) -> String {
    match kind {
        EntryKind::Tree => graft_model::TreeId(id).to_string(),
        EntryKind::Submodule => id.to_hex(),
        _ => graft_model::BlobId(id).to_string(),
    }
}

fn entry_json(e: &EntryView) -> Value {
    json!({ "path": e.path.as_str(), "kind": entry_kind(e.kind), "id": entry_id(e.kind, e.id) })
}

fn change_json(c: &Change) -> Value {
    match c {
        Change::Put { path, kind, id } => json!({
            "op": "put", "path": path.as_str(), "kind": entry_kind(*kind), "id": entry_id(*kind, *id),
        }),
        Change::Delete { path } => json!({ "op": "delete", "path": path.as_str() }),
    }
}

fn summary_json(s: &ChangesetSummary) -> Value {
    json!({
        "id": s.id.to_string(),
        "author": { "name": s.author.name, "email": s.author.email, "kind": author_kind(s.author.kind) },
        "authored_at": time_json(s.authored_at),
        "subject": s.subject,
        "line_pos": s.line_pos,
    })
}

fn time_json(t: Timestamp) -> Value {
    json!({ "ms": t.ms, "offset_min": t.offset_min })
}

fn entry_kind(k: EntryKind) -> &'static str {
    match k {
        EntryKind::File => "file",
        EntryKind::Exec => "exec",
        EntryKind::Symlink => "symlink",
        EntryKind::Tree => "tree",
        EntryKind::Submodule => "submodule",
    }
}

fn author_kind(k: AuthorKind) -> &'static str {
    match k {
        AuthorKind::Human => "human",
        AuthorKind::Agent => "agent",
        AuthorKind::Bot => "bot",
        AuthorKind::Ci => "ci",
    }
}

fn draft_state(s: DraftState) -> &'static str {
    match s {
        DraftState::Draft => "draft",
        DraftState::Landed => "landed",
        DraftState::Superseded => "superseded",
        DraftState::Abandoned => "abandoned",
    }
}

async fn attestations(
    State(app): State<Arc<App>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Answer {
    let cx = cx(&app, &headers, "GetAttestations")?;
    let q = Query::parse(raw)?;
    let changeset = q.get("changeset").filter(|s| !s.is_empty());
    let line = q.get("line").filter(|s| !s.is_empty());
    let selector = match (changeset, line) {
        (Some(id), None) => AttestationSelector::ByChangeset(
            ChangesetId::parse(id).map_err(|_| bad("changeset", "not a changeset id"))?,
        ),
        (None, Some(line)) => {
            let from_pos = q
                .num("from_pos")?
                .ok_or_else(|| bad("from_pos", "required"))?;
            let to_pos = q.num("to_pos")?.ok_or_else(|| bad("to_pos", "required"))?;
            AttestationSelector::ByLineRange {
                line: line.to_owned(),
                from_pos,
                to_pos,
            }
        }
        _ => {
            return Err(bad(
                "selector",
                "give changeset or line, from_pos and to_pos",
            ));
        }
    };
    let mut kinds = Vec::new();
    for kind in q.all("kind").filter(|k| !k.is_empty()) {
        kinds.push(match kind {
            "check" => AttestKind::Check,
            "review" => AttestKind::Review,
            "approval" => AttestKind::Approval,
            _ => return Err(bad("kind", "expected check, review or approval")),
        });
    }
    let mut page = q.page()?;
    page.limit = crate::clamp_review_page(Some(page.limit));
    let req = GetAttestations {
        repo: repo.clone(),
        selector,
        kinds,
        page,
    };
    let page = app
        .call(
            "GetAttestations",
            &repo,
            cx,
            false,
            move |e, cx| async move { e.get_attestations(&cx, req).await },
        )
        .await?;
    ok(json!({
        "attestations": page.attestations.iter().map(attestation_json).collect::<Vec<_>>(),
        "next": page.next,
    }))
}

fn attestation_json(a: &Attestation) -> Value {
    json!({
        "changeset": a.changeset.to_string(),
        "kind": match a.kind {
            AttestKind::Check => "check",
            AttestKind::Review => "review",
            AttestKind::Approval => "approval",
            _ => "unknown",
        },
        "check": a.check,
        "status": match a.status {
            AttestStatus::Pass => "pass",
            AttestStatus::Fail => "fail",
            AttestStatus::Pending => "pending",
            _ => "unknown",
        },
        "by": a.by,
        "url": a.url,
    })
}

async fn land_checks(
    State(app): State<Arc<App>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Answer {
    let cx = cx(&app, &headers, "GetLandChecks")?;
    let q = Query::parse(raw)?;
    let line = q
        .get("line")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad("line", "required"))?
        .to_owned();
    let from_pos = q.num("from_pos")?.unwrap_or(0);
    let mut page = q.page()?;
    page.limit = crate::clamp_review_page(Some(page.limit));
    let req = GetLandChecks {
        repo: repo.clone(),
        line,
        from_pos,
        page,
    };
    let page = app
        .call("GetLandChecks", &repo, cx, false, move |e, cx| async move {
            e.get_land_checks(&cx, req).await
        })
        .await?;
    ok(json!({
        "rows": page.rows.iter().map(|row| json!({
            "pos": row.pos,
            "changeset": row.changeset.to_string(),
            "state": match row.state {
                LandCheckState::Pending => "pending",
                LandCheckState::Good => "good",
                LandCheckState::Bad => "bad",
                LandCheckState::Reverted => "reverted",
                _ => "unknown",
            },
            "revert": row.revert.map(|id| id.to_string()),
        })).collect::<Vec<_>>(),
        "next": page.next,
    }))
}

async fn review_queue(
    State(app): State<Arc<App>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Answer {
    let cx = cx(&app, &headers, "ReviewQueue")?;
    let q = Query::parse(raw)?;
    if q.get("page_token").is_some() {
        return Err(bad("page_token", "review-queue resumes with since_lsn"));
    }
    let line = q
        .get("line")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad("line", "required"))?
        .to_owned();
    let since_lsn = q.num("since_lsn")?.unwrap_or(0);
    let path_prefix = q.path()?;
    let max_lines = q.num("max_lines")?;
    let mut page = q.page()?;
    page.limit = crate::clamp_review_page(Some(page.limit));
    let req = ReviewQueue {
        repo: repo.clone(),
        line,
        since_lsn,
        path_prefix,
        max_lines,
        page,
    };
    let page = app
        .call("ReviewQueue", &repo, cx, false, move |e, cx| async move {
            e.review_queue(&cx, req).await
        })
        .await?;
    ok(json!({
        "items": page.items.iter().map(|item| json!({
            "changeset": item.changeset.to_string(),
            "pos": item.pos,
            "lsn": item.lsn,
            "author": {
                "name": item.author.name,
                "email": item.author.email,
                "kind": author_kind(item.author.kind),
            },
            "paths_touched": item.paths_touched,
            "lines_changed": item.lines_changed,
        })).collect::<Vec<_>>(),
        "next_lsn": page.next_lsn,
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn query_strings_decode() {
        let q = Query::parse(Some("path=a%2Fb&path=c+d&recursive&limit=5".into())).expect("ok");
        assert_eq!(q.all("path").collect::<Vec<_>>(), ["a/b", "c d"]);
        assert!(q.flag("recursive").expect("flag"));
        assert_eq!(q.num::<u32>("limit").expect("num"), Some(5));
        assert!(Query::parse(Some("a=%zz".into())).is_err());
        assert!(Query::parse(Some("a=%e2%82".into())).is_err());
        assert!(q.num::<u32>("path").is_err());
    }

    use graft_auth::{Claims, SigningKey};
    use graft_engine::policy::{LineChecks, LineConfig};
    use graft_engine::{Engine, EngineConfig};
    use graft_model::{Author, AuthorKind, EntryKind, Metadata, ObjectKind, SystemClock};
    use graft_proto::types::MAX_REVIEW_PAGE;
    use graft_proto::{Frame, ObjectApi as _, Principal, PrincipalKind};
    use graft_store::MetaStore as _;
    use graft_store_fs::{FsMode, FsObjectStore};
    use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};
    use std::sync::OnceLock;

    pub(crate) struct Harness {
        pub(crate) app: Arc<App>,
        pub(crate) token: String,
        _dir: tempfile::TempDir,
    }

    fn telemetry() -> Arc<graft_telemetry::Telemetry> {
        static TELEMETRY: OnceLock<Arc<graft_telemetry::Telemetry>> = OnceLock::new();
        TELEMETRY
            .get_or_init(|| {
                Arc::new(
                    graft_telemetry::init(&graft_telemetry::TelemetryConfig {
                        service: "graft-server-http-tests".into(),
                        json_logs: false,
                        metrics_addr: None,
                        otlp_endpoint: None,
                    })
                    .expect("initialize telemetry"),
                )
            })
            .clone()
    }

    fn checked_main() -> EngineConfig {
        let mut line = LineConfig::direct("main");
        line.checks = LineChecks::PostLand {
            required: vec!["ci/test".into()],
            retries: 0,
        };
        EngineConfig {
            lines: vec![line],
            ..EngineConfig::default()
        }
    }

    async fn app() -> Harness {
        app_with(EngineConfig::default()).await
    }

    async fn app_with(config: EngineConfig) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let meta = std::sync::Arc::new(
            SqliteMetaStore::open(SqliteConfig::new(dir.path().join("meta.db"))).unwrap(),
        );
        let objects = std::sync::Arc::new(
            FsObjectStore::open(dir.path().join("objects"), FsMode::Authoritative).unwrap(),
        );
        let engine = Arc::new(Engine::new(
            meta.clone(),
            objects,
            std::sync::Arc::new(SystemClock),
            config,
        ));
        meta.create_repo("graft").await.unwrap();
        let signing = SigningKey::generate().unwrap();
        let token = graft_auth::mint(
            &signing,
            &Claims {
                principal: Principal {
                    subject: "reviewer".into(),
                    kind: PrincipalKind::Human,
                    session: None,
                    work_item: None,
                    scopes: vec!["repo:graft:read".into()],
                },
                exp: i64::MAX / 2,
                jti: graft_auth::new_jti().unwrap(),
            },
        )
        .unwrap();
        let app = Arc::new(App::new(
            engine,
            signing,
            crate::admission::Admission::new(vec![], 0.0, 0),
            telemetry(),
            3_600,
            crate::watch::Watches::new(crate::watch::WatchLimits {
                max_wait: std::time::Duration::from_secs(1),
                max_lifetime: std::time::Duration::from_secs(1),
                per_principal: 1,
                total: 1,
            }),
            crate::app::PutBudget::new(1 << 20, 1 << 20),
            crate::denylist::Denylist::open(&dir.path().join("meta.db")).unwrap(),
            Arc::new(crate::exports::Exports::open(&dir.path().join("meta.db")).unwrap()),
        ));
        app.set_ready(true);
        Harness {
            app,
            token: token.expose().to_owned(),
            _dir: dir,
        }
    }

    async fn hit(app: Arc<App>, token: &str, path: &str) -> (u16, serde_json::Value) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, facade_router(app)).await.unwrap();
        });
        let response = reqwest::Client::new()
            .get(format!("http://{addr}{path}"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = response.bytes().await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        server.abort();
        (status, json)
    }

    #[tokio::test]
    async fn review_routes_return_a_bounded_page_and_typed_errors() {
        let harness = app().await;
        let cases = [
            ("/v1/repos/graft/attestations", "invalid_change"),
            (
                "/v1/repos/graft/attestations?changeset=not-an-id",
                "invalid_change",
            ),
            (
                "/v1/repos/graft/attestations?kind=nope&line=main&from_pos=0&to_pos=0",
                "invalid_change",
            ),
            ("/v1/repos/graft/land-checks", "invalid_change"),
            (
                "/v1/repos/graft/land-checks?line=no%20space",
                "invalid_change",
            ),
            ("/v1/repos/graft/review-queue", "invalid_change"),
            (
                "/v1/repos/graft/review-queue?line=main&page_token=abc",
                "invalid_change",
            ),
            (
                "/v1/repos/graft/review-queue?line=main&path=/abs",
                "invalid_path",
            ),
        ];
        for (path, kind) in cases {
            let (status, body) = hit(harness.app.clone(), &harness.token, path).await;
            assert_eq!(status, 400, "{path}: {body}");
            assert_eq!(body["error"]["kind"], kind, "{path}: {body}");
        }
        // Two rows per route. limit=1 must return one row and a resume
        // token. The over-cap request is clamped before the engine; the
        // clamp itself is `clamp_review_page`.
        let harness = seeded_review_harness().await;
        let over = MAX_REVIEW_PAGE + 1;
        let routes = [
            (
                "attestations",
                "/v1/repos/graft/attestations?line=main&from_pos=0&to_pos=9&limit=",
                "attestations",
                "next",
            ),
            (
                "rows",
                "/v1/repos/graft/land-checks?line=main&from_pos=0&limit=",
                "rows",
                "next",
            ),
            (
                "items",
                "/v1/repos/graft/review-queue?line=main&since_lsn=0&limit=",
                "items",
                "next_lsn",
            ),
        ];
        for (name, template, list_key, next_key) in routes {
            let one = format!("{template}1");
            let (status, body) = hit(harness.app.clone(), &harness.token, &one).await;
            assert_eq!(status, 200, "{name}: {body}");
            let list = body[list_key]
                .as_array()
                .unwrap_or_else(|| panic!("{name}: {body}"));
            assert_eq!(
                list.len(),
                1,
                "{name} limit=1 returned the whole set: {body}"
            );
            assert!(
                !body[next_key].is_null(),
                "{name} limit=1 has no resume token: {body}"
            );
            let wide = format!("{template}{over}");
            let (status, body) = hit(harness.app.clone(), &harness.token, &wide).await;
            assert_eq!(status, 200, "{name}: {body}");
            let list = body[list_key]
                .as_array()
                .unwrap_or_else(|| panic!("{name}: {body}"));
            assert_eq!(list.len(), 2, "{name} over-cap page: {body}");
            assert!(
                body[next_key].is_null(),
                "{name} over-cap invented a next: {body}"
            );
        }
    }

    /// Two landed changesets on `main`, each with a check attestation.
    /// A harness whose `main` line already has two review rows.
    pub(crate) async fn seeded_review_harness() -> Harness {
        let harness = app_with(checked_main()).await;
        seed_review_rows(&harness).await;
        harness
    }

    async fn seed_review_rows(harness: &Harness) {
        let engine = &harness.app.engine;
        let cx = graft_proto::CallContext {
            principal: Principal {
                subject: "writer".into(),
                kind: PrincipalKind::Human,
                session: None,
                work_item: None,
                scopes: vec!["repo:graft:admin".into(), "repo:graft:attest".into()],
            },
            request_id: "seed".into(),
            deadline_ms: None,
        };
        let mut parent = None;
        // Three commits: the line base, then two landings.
        for (n, bytes) in [
            (0u8, b"base".as_slice()),
            (1, b"one".as_slice()),
            (2, b"two".as_slice()),
        ] {
            let frame = Frame::encode(ObjectKind::Blob, bytes);
            engine
                .put_objects(
                    &cx,
                    PutObjects {
                        repo: "graft".into(),
                        blobs: vec![frame],
                    },
                )
                .await
                .unwrap();
            let id = ObjectId::hash(ObjectKind::Blob, bytes);
            let created = engine
                .create_changeset(
                    &cx,
                    CreateChangeset {
                        repo: "graft".into(),
                        parents: parent.into_iter().collect(),
                        changes: vec![Change::Put {
                            path: RepoPath::new(&format!("f{n}")).unwrap(),
                            kind: EntryKind::File,
                            id,
                        }],
                        author: Author {
                            name: "Tess".into(),
                            email: "tess@example.com".into(),
                            kind: AuthorKind::Human,
                        },
                        authored_at: Timestamp {
                            ms: 1_700_000_000_000,
                            offset_min: 0,
                        },
                        message: format!("change {n}"),
                        metadata: Metadata::new(),
                        inline_blobs: Vec::new(),
                        workspace: None,
                    },
                )
                .await
                .unwrap();
            if parent.is_none() {
                // The line starts at this changeset. That move is not a
                // landing, so it writes no review or land-check row.
                engine
                    .create_line(
                        &cx,
                        CreateLine {
                            repo: "graft".into(),
                            name: "main".into(),
                            from: created.id,
                        },
                    )
                    .await
                    .unwrap();
            } else {
                engine
                    .land(
                        &cx,
                        Land {
                            repo: "graft".into(),
                            head: created.id,
                            line: "main".into(),
                            squash: false,
                            merge: MergeMode::File,
                            deadline_ms: None,
                            merged_from: None,
                        },
                    )
                    .await
                    .unwrap();
            }
            if parent.is_some() {
                engine
                    .attest_v2(
                        &cx,
                        AttestV2 {
                            repo: "graft".into(),
                            changeset: created.id,
                            kind: AttestKind::Check,
                            check: "ci/test".into(),
                            status: AttestStatus::Pass,
                            by: None,
                            url: String::new(),
                        },
                    )
                    .await
                    .unwrap();
            }
            parent = Some(created.id);
        }
    }
}
