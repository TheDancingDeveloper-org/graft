//! The graft LSP server (`docs/design.md` §10.2, work item G3.6).
//!
//! Runs next to the editor's language server and adds source-control
//! features: blame hovers, history code lenses, conflict and staleness
//! diagnostics, commands, and `graft:` virtual documents.
//!
//! The server holds no state of its own beyond what the editor has open: it
//! reads the `graft-client` working copy (reloading it when `.graft/`
//! changes on disk, for instance after a `graft commit` in a terminal) and
//! calls the API traits. Every answer is one bounded API call or a few:
//! blame for one line, a path-filtered log page, a path-filtered diff.
//!
//! Requests are handled one at a time, in order. A write command (commit,
//! land, sync) holds the loop until it finishes, renewing the workspace
//! lease while it runs.
//!
//! | LSP | graft |
//! | --- | --- |
//! | `textDocument/hover` | blame for the line: changeset, author and kind, subject, state, work item |
//! | `textDocument/codeLens` | "changed N times, last by X"; "base is K changesets behind `<line>`" |
//! | `textDocument/publishDiagnostics` | conflict regions after a failed sync or land; files outside the sparse slice; regions changed upstream since the base |
//! | `workspace/executeCommand` | [`COMMANDS`] |
//! | [`READ_FILE_REQUEST`] | a file at any changeset, by `graft:` URI ([`GraftUri`]) |

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use graft_client::workcopy::WorkingCopy;
use graft_model::{Author, ChangesetId, RepoPath};
use graft_proto::api::{ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::context::{CallContext, Principal, PrincipalKind};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};

mod commands;
mod features;
mod render;
mod rpc;
mod uri;

pub use uri::{GraftUri, SCHEME, file_path, file_uri};

use rpc::{Frame, RpcError, code};

/// Custom LSP request for reading a file at a changeset. Its parameters are
/// `{"uri": "graft://<repo>/<rev>/<path>"}`; the answer is
/// `{"uri", "changeset", "path", "text"}`.
pub const READ_FILE_REQUEST: &str = "graft/readFile";

/// Commands offered through `workspace/executeCommand`. Each takes one
/// optional argument, an object:
///
/// - `graft.commit` `{message, amend?, author?: {name, email, kind?}, metadata?}`
/// - `graft.land` `{squash?}`
/// - `graft.sync` (no argument)
/// - `graft.diffBase` `{uri?, page?}`: the stack's diff from the base, and
///   the base's `graft:` URI for `uri`
/// - `graft.blameFile` `{uri}` (or the URI as a string)
pub const COMMANDS: &[&str] = &[
    "graft.commit",
    "graft.land",
    "graft.sync",
    "graft.diffBase",
    "graft.blameFile",
];

/// The largest file [`READ_FILE_REQUEST`] returns.
pub const MAX_READ_BYTES: usize = 8 << 20;

/// The largest serialized JSON-RPC response. Feature-specific pagination and
/// caps should stay below this; this is the final boundary backstop.
pub const MAX_RESPONSE_BYTES: usize = 8 << 20;

/// A response-producing LSP surface and its externally visible bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputSpec {
    /// Request, command, or notification name.
    pub name: &'static str,
    /// How an oversized list or content result is continued or refused.
    pub bound: OutputBound,
}

/// A response's bounding strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputBound {
    /// The caller follows a page token.
    Paginated,
    /// The result is clipped, truncated, or refused with `limit_exceeded`.
    Capped,
}

/// Every LSP surface that returns a list or repository content.
pub const OUTPUTS: &[OutputSpec] = &[
    OutputSpec {
        name: "textDocument/hover",
        bound: OutputBound::Capped,
    },
    OutputSpec {
        name: "textDocument/codeLens",
        bound: OutputBound::Capped,
    },
    OutputSpec {
        name: "textDocument/publishDiagnostics",
        bound: OutputBound::Capped,
    },
    OutputSpec {
        name: "graft.diffBase",
        bound: OutputBound::Paginated,
    },
    OutputSpec {
        name: "graft.blameFile",
        bound: OutputBound::Capped,
    },
    OutputSpec {
        name: READ_FILE_REQUEST,
        bound: OutputBound::Capped,
    },
];

/// How the input ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// `exit` after `shutdown`: the editor closed the server normally.
    Clean,
    /// `exit` without `shutdown`, or the input closed.
    Unclean,
}

/// An LSP server over an API and an optional working copy.
pub struct LspServer<A> {
    api: A,
    repo: String,
    workcopy: Option<WorkingCopy>,
    /// The `.graft/` files as last loaded, to notice changes on disk.
    stamp: Option<Stamp>,
    principal: Principal,
    author: Option<Author>,
    /// Open documents.
    docs: BTreeSet<String>,
    /// The conflict a sync or land last stopped on, until one succeeds.
    conflict: Option<Conflict>,
    /// URIs whose last published diagnostics were not empty.
    published: BTreeSet<String>,
    initialized: bool,
    shut_down: bool,
    lens_refresh: bool,
    next_request: u64,
    /// Messages to send after the current one is handled.
    outbox: Vec<Value>,
}

impl<A> std::fmt::Debug for LspServer<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspServer")
            .field("repo", &self.repo)
            .field("workcopy", &self.workcopy.as_ref().map(WorkingCopy::root))
            .field("docs", &self.docs.len())
            .finish_non_exhaustive()
    }
}

/// The paths a failed sync or land conflicted on.
#[derive(Clone, Debug)]
struct Conflict {
    onto: ChangesetId,
    paths: BTreeSet<RepoPath>,
}

/// What an open document is.
enum Target {
    /// A file in the working copy's tree.
    Local(RepoPath),
    /// A `graft:` URI.
    Virtual(GraftUri),
    /// Anything else: no graft features.
    Other,
}

/// Size, mtime and inode of each `.graft/` state file.
type Stamp = Vec<Option<(u64, i64, u64)>>;

const STATE_FILES: [&str; 4] = ["config", "state", "sparse", "index"];

fn stamp(root: &Path) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    let dir = root.join(graft_client::workcopy::GRAFT_DIR);
    STATE_FILES
        .iter()
        .map(|f| {
            std::fs::metadata(dir.join(f))
                .ok()
                .map(|m| (m.len(), m.mtime() * 1_000_000_000 + m.mtime_nsec(), m.ino()))
        })
        .collect()
}

impl<A: ObjectApi + RepoApi + WorkspaceApi> LspServer<A> {
    /// Builds a server for `repo`. Without a working copy only `graft:`
    /// documents get features; [`LspServer::serve`] also looks for one at
    /// the editor's root folder.
    pub fn new(api: A, repo: impl Into<String>, workcopy: Option<WorkingCopy>) -> Self {
        let stamp = workcopy.as_ref().map(|wc| stamp(wc.root()));
        Self {
            api,
            repo: repo.into(),
            workcopy,
            stamp,
            principal: Principal {
                subject: std::env::var("USER").unwrap_or_else(|_| "graft-lsp".to_owned()),
                kind: PrincipalKind::Human,
                session: None,
                work_item: None,
                scopes: Vec::new(),
            },
            author: None,
            docs: BTreeSet::new(),
            conflict: None,
            published: BTreeSet::new(),
            initialized: false,
            shut_down: false,
            lens_refresh: false,
            next_request: 1,
            outbox: Vec::new(),
        }
    }

    /// Sets the principal carried in each call's context.
    pub fn with_principal(mut self, principal: Principal) -> Self {
        self.principal = principal;
        self
    }

    /// Sets the default author of `graft.commit` (otherwise taken from
    /// `GRAFT_AUTHOR_NAME`, `GRAFT_AUTHOR_EMAIL`, `GRAFT_AUTHOR_KIND` and
    /// `USER`, as the CLI does).
    pub fn with_author(mut self, author: Author) -> Self {
        self.author = Some(author);
        self
    }

    /// The API in use.
    pub fn api(&self) -> &A {
        &self.api
    }

    /// The working copy, if any.
    pub fn workcopy(&self) -> Option<&WorkingCopy> {
        self.workcopy.as_ref()
    }

    /// Serves LSP over stdin and stdout until `exit` or the end of input.
    pub async fn serve_stdio(self) -> std::io::Result<Ended> {
        self.serve(tokio::io::stdin(), tokio::io::stdout()).await
    }

    /// Serves LSP over `input` and `output` until `exit` or the end of
    /// input. A message that is not JSON, or too large, gets an error
    /// response; a broken frame ends the session with an error.
    pub async fn serve<R, W>(mut self, input: R, mut output: W) -> std::io::Result<Ended>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut input = BufReader::new(input);
        loop {
            let Some(frame) = rpc::read_frame(&mut input).await? else {
                return Ok(Ended::Unclean);
            };
            let ended = match frame {
                Frame::TooLarge => {
                    let e = RpcError::new(code::INVALID_REQUEST, "the message is too large");
                    self.outbox.push(rpc::response(Value::Null, Err(e)));
                    None
                }
                Frame::Body(body) => match serde_json::from_slice::<Value>(&body) {
                    Ok(msg) => self.handle(msg).await,
                    Err(_) => {
                        let e = RpcError::new(code::PARSE_ERROR, "the message is not JSON");
                        self.outbox.push(rpc::response(Value::Null, Err(e)));
                        None
                    }
                },
            };
            for msg in std::mem::take(&mut self.outbox) {
                rpc::write_message(&mut output, &msg).await?;
            }
            if let Some(ended) = ended {
                return Ok(ended);
            }
        }
    }

    /// Handles one message. `Some` when the session is over.
    async fn handle(&mut self, msg: Value) -> Option<Ended> {
        let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let Some(method) = method else {
            // A response to one of our requests (`workspace/codeLens/refresh`),
            // or junk; neither needs an answer.
            return None;
        };
        let Some(id) = msg.get("id").cloned() else {
            return self.notification(&method, params).await;
        };
        let result = self.request(&method, params).await;
        self.outbox.push(bounded_response(id, result));
        None
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        if method == "initialize" {
            return Ok(self.initialize(&params));
        }
        if !self.initialized {
            return Err(RpcError::new(
                code::SERVER_NOT_INITIALIZED,
                "initialize first",
            ));
        }
        if self.shut_down {
            return Err(RpcError::new(
                code::INVALID_REQUEST,
                "the server is shut down",
            ));
        }
        match method {
            "shutdown" => {
                self.shut_down = true;
                Ok(Value::Null)
            }
            "textDocument/hover" => self.hover(&params).await,
            "textDocument/codeLens" => self.code_lens(&params).await,
            "workspace/executeCommand" => self.execute_command(&params).await,
            READ_FILE_REQUEST => self.read_file(&params).await,
            _ => Err(RpcError::new(
                code::METHOD_NOT_FOUND,
                format!("unknown method {method}"),
            )),
        }
    }

    async fn notification(&mut self, method: &str, params: Value) -> Option<Ended> {
        match method {
            "exit" => {
                return Some(if self.shut_down {
                    Ended::Clean
                } else {
                    Ended::Unclean
                });
            }
            _ if !self.initialized || self.shut_down => {}
            "textDocument/didOpen" | "textDocument/didSave" => {
                if let Some(uri) = doc_uri(&params) {
                    self.docs.insert(uri.clone());
                    self.publish_all(Some(&uri)).await;
                }
            }
            "textDocument/didClose" => {
                if let Some(uri) = doc_uri(&params) {
                    self.docs.remove(&uri);
                    if self.published.remove(&uri) {
                        self.push_publish(&uri, Vec::new());
                    }
                }
            }
            _ => {}
        }
        None
    }

    fn push_publish(&mut self, uri: &str, diagnostics: Vec<Value>) {
        self.outbox.push(bounded_publish(uri, diagnostics));
    }

    fn initialize(&mut self, params: &Value) -> Value {
        self.initialized = true;
        self.lens_refresh = params
            .pointer("/capabilities/workspace/codeLens/refreshSupport")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if self.workcopy.is_none()
            && let Some(root) = root_folder(params)
            && let Ok(wc) = WorkingCopy::open(&root)
        {
            if wc.config().repo == self.repo {
                self.stamp = Some(stamp(wc.root()));
                self.workcopy = Some(wc);
            } else {
                self.log(&format!(
                    "the working copy at {} is of repository {:?}, not {:?}; ignoring it",
                    root.display(),
                    wc.config().repo,
                    self.repo
                ));
            }
        }
        json!({
            "capabilities": {
                "textDocumentSync": {
                    "openClose": true,
                    "change": 0,
                    "save": {"includeText": false},
                },
                "hoverProvider": true,
                "codeLensProvider": {"resolveProvider": false},
                "executeCommandProvider": {"commands": COMMANDS},
                "experimental": {
                    "graft": {"readFile": READ_FILE_REQUEST, "scheme": SCHEME},
                },
            },
            "serverInfo": {"name": "graft-lsp", "version": env!("CARGO_PKG_VERSION")},
        })
    }

    /// A fresh call context.
    fn cx(&self) -> CallContext {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        CallContext {
            principal: self.principal.clone(),
            request_id: graft_client::request_id("lsp", n),
            deadline_ms: None,
        }
    }

    /// Reloads the working copy when its `.graft/` files changed on disk.
    fn refresh_workcopy(&mut self) {
        let Some(wc) = &self.workcopy else {
            return;
        };
        let now = stamp(wc.root());
        if self.stamp.as_ref() == Some(&now) {
            return;
        }
        match WorkingCopy::open(wc.root()) {
            Ok(wc) => {
                self.workcopy = Some(wc);
                self.stamp = Some(now);
            }
            Err(e) => self.log(&format!("cannot reload the working copy: {e}")),
        }
    }

    /// Records the `.graft/` files after this server changed them.
    fn restamp(&mut self) {
        self.stamp = self.workcopy.as_ref().map(|wc| stamp(wc.root()));
    }

    /// What `uri` names.
    fn target(&self, uri: &str) -> Target {
        if uri.starts_with("graft:") {
            return match GraftUri::parse(uri) {
                Ok(g) => Target::Virtual(g),
                Err(_) => Target::Other,
            };
        }
        match (&self.workcopy, file_path(uri)) {
            (Some(wc), Some(path)) => match repo_path(wc.root(), &path) {
                Some(p) => Target::Local(p),
                None => Target::Other,
            },
            _ => Target::Other,
        }
    }

    /// Sends a message to the editor's log.
    fn log(&mut self, message: &str) {
        self.outbox.push(rpc::notification(
            "window/logMessage",
            json!({"type": 3, "message": message}),
        ));
    }

    /// Shows a message to the person (1 error, 3 info).
    fn show(&mut self, kind: u8, message: &str) {
        self.outbox.push(rpc::notification(
            "window/showMessage",
            json!({"type": kind, "message": message}),
        ));
    }

    /// Asks the editor to fetch code lenses again, if it can.
    fn refresh_lenses(&mut self) {
        if self.lens_refresh {
            let id = self.next_request;
            self.next_request += 1;
            self.outbox
                .push(rpc::request(id, "workspace/codeLens/refresh", Value::Null));
        }
    }
}

/// The repository path of `path` inside the working copy at `root`. `None`
/// outside it, inside `.graft/`, or for a name graft can't store.
fn repo_path(root: &Path, path: &Path) -> Option<RepoPath> {
    let rel = match path.strip_prefix(root) {
        Ok(rel) => rel.to_owned(),
        Err(_) => {
            let root = root.canonicalize().ok()?;
            let path = canonical_parent(path)?;
            path.strip_prefix(&root).ok()?.to_owned()
        }
    };
    let text = rel.to_str()?;
    if text.is_empty() {
        return None;
    }
    let p = RepoPath::new(text).ok()?;
    let first = p.components().next()?;
    (first != graft_client::workcopy::GRAFT_DIR).then_some(p)
}

/// `path` with its parent directory resolved (the file itself may not exist
/// yet, or be a symlink to keep).
fn canonical_parent(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?;
    Some(path.parent()?.canonicalize().ok()?.join(name))
}

/// The editor's root folder from `initialize`.
fn root_folder(params: &Value) -> Option<PathBuf> {
    let uri = params
        .pointer("/workspaceFolders/0/uri")
        .and_then(Value::as_str)
        .or_else(|| params.get("rootUri").and_then(Value::as_str))?;
    file_path(uri)
}

fn doc_uri(params: &Value) -> Option<String> {
    params
        .pointer("/textDocument/uri")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn publish(uri: &str, diagnostics: Vec<Value>) -> Value {
    rpc::notification(
        "textDocument/publishDiagnostics",
        json!({"uri": uri, "diagnostics": diagnostics}),
    )
}

/// A diagnostics notification small enough to send. An oversized payload is
/// replaced by one typed diagnostic naming the cap, so the notification
/// itself stays under [`MAX_RESPONSE_BYTES`].
fn bounded_publish(uri: &str, diagnostics: Vec<Value>) -> Value {
    let note = publish(uri, diagnostics);
    if serde_json::to_vec(&note).is_ok_and(|body| body.len() <= MAX_RESPONSE_BYTES) {
        return note;
    }
    publish(
        uri,
        vec![json!({
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 1, "character": 0},
            },
            "severity": 1,
            "source": "graft",
            "code": "limit_exceeded",
            "message": format!(
                "diagnostics exceeded {MAX_RESPONSE_BYTES} bytes and were not published"
            ),
            "data": {"error": "limit_exceeded", "max_bytes": MAX_RESPONSE_BYTES},
        })],
    )
}

fn bounded_response(id: Value, result: Result<Value, RpcError>) -> Value {
    let response = rpc::response(id.clone(), result);
    if serde_json::to_vec(&response).is_ok_and(|body| body.len() <= MAX_RESPONSE_BYTES) {
        return response;
    }
    rpc::response(
        id,
        Err(RpcError::failed("response exceeds the LSP size limit")
            .with_data(json!({"error": "limit_exceeded", "max_bytes": MAX_RESPONSE_BYTES}))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_paths_are_inside_the_working_copy_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let p = |s: &str| repo_path(root, &root.join(s)).map(|p| p.as_str().to_owned());
        assert_eq!(p("src/lib.rs").as_deref(), Some("src/lib.rs"));
        assert_eq!(p(".graft/state"), None);
        assert_eq!(p(""), None);
        assert_eq!(repo_path(root, Path::new("/elsewhere/x")), None);
        // A path through a symlinked root still maps.
        let link = dir.path().with_extension("link");
        std::os::unix::fs::symlink(root, &link).unwrap();
        assert_eq!(
            repo_path(root, &link.join("src/lib.rs")).map(|p| p.as_str().to_owned()),
            Some("src/lib.rs".to_owned())
        );
        std::fs::remove_file(link).unwrap();
    }

    #[test]
    fn every_request_surface_refuses_an_oversized_result() {
        let huge = "x".repeat(MAX_RESPONSE_BYTES);
        let cases = [
            (
                "textDocument/hover",
                json!({"contents": {"kind": "markdown", "value": huge}}),
            ),
            (
                "textDocument/codeLens",
                json!([{"command": {"title": huge}}]),
            ),
            (
                "graft.diffBase",
                json!({"files": [{"patch": huge, "truncated": true}], "next": "100"}),
            ),
            (
                "graft.blameFile",
                json!({"ranges": [{"subject": huge}], "truncated": true}),
            ),
            (READ_FILE_REQUEST, json!({"text": huge})),
        ];
        for (name, value) in cases {
            let response = bounded_response(json!(name), Ok(value));
            assert_eq!(response["id"], name);
            assert_eq!(response["error"]["data"]["error"], "limit_exceeded");
            assert_eq!(response["error"]["data"]["max_bytes"], MAX_RESPONSE_BYTES);
            assert!(
                serde_json::to_vec(&response).unwrap().len() <= MAX_RESPONSE_BYTES,
                "{name} exceeded the response cap"
            );
        }
    }

    #[test]
    fn oversized_diagnostics_become_a_typed_limit_notification() {
        let huge = vec![json!({"message": "x".repeat(MAX_RESPONSE_BYTES)})];
        let note = bounded_publish("file:///src/lib.rs", huge);
        assert!(note.get("id").is_none(), "a notification has no id");
        assert_eq!(note["method"], "textDocument/publishDiagnostics");
        let diag = &note["params"]["diagnostics"];
        assert_eq!(diag.as_array().unwrap().len(), 1);
        assert_eq!(diag[0]["code"], "limit_exceeded");
        assert_eq!(diag[0]["data"]["max_bytes"], MAX_RESPONSE_BYTES);
        assert!(serde_json::to_vec(&note).unwrap().len() <= MAX_RESPONSE_BYTES);

        let small = bounded_publish("file:///src/lib.rs", vec![json!({"message": "ok"})]);
        assert_eq!(small["params"]["diagnostics"][0]["message"], "ok");
    }

    #[test]
    fn the_output_inventory_covers_every_list_and_content_surface() {
        let names: BTreeSet<&str> = OUTPUTS.iter().map(|o| o.name).collect();
        for required in [
            "textDocument/hover",
            "textDocument/codeLens",
            "textDocument/publishDiagnostics",
            "graft.diffBase",
            "graft.blameFile",
            READ_FILE_REQUEST,
        ] {
            assert!(names.contains(required), "{required} has no bound");
        }
        assert_eq!(names.len(), OUTPUTS.len(), "duplicate output names");
        let bounded_commands: BTreeSet<&str> = OUTPUTS
            .iter()
            .map(|output| output.name)
            .filter(|name| COMMANDS.contains(name))
            .collect();
        assert_eq!(
            bounded_commands,
            BTreeSet::from(["graft.blameFile", "graft.diffBase"]),
            "every content-returning command must be inventoried"
        );
        assert!(
            OUTPUTS
                .iter()
                .all(|o| matches!(o.bound, OutputBound::Paginated | OutputBound::Capped))
        );
    }
}
