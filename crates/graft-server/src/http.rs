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
//!
//! `log` and `blame` answer 501 until the engine wires `graft-index`
//! (G1.10).

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use graft_model::{AuthorKind, Change, ChangesetId, EntryKind, ObjectId, RepoPath, Timestamp};
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, RepoApi as _};
use serde_json::{Value, json};

use crate::app::{App, CallError, error_name};

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

fn cx(app: &App, headers: &HeaderMap, op: &'static str) -> Result<CallContext, CallError> {
    app.authenticate(|k| headers.get(k).and_then(|v| v.to_str().ok()), false)
        .map_err(|e| {
            app.refused(op, &e);
            CallError::Api(e)
        })
}

/// A changeset id in text form, or else a ref name resolved now.
async fn at(app: &App, cx: &CallContext, repo: &str, text: &str) -> Result<ChangesetId, CallError> {
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

#[cfg(test)]
mod tests {
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
}
