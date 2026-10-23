//! The LSP conformance rig: the real engine over the in-memory store,
//! served by the testkit gRPC server, and a scripted LSP client.
//!
//! Land, which the engine does not build yet, is a fake PR bridge, as in
//! the MCP tests: it checks the stack sits on the line tip and answers
//! `Queued` with a pull request URL. The testkit server's callers carry a
//! bare `admin` scope the engine doesn't know, so the rig calls the engine
//! as the repository's admin.

#![allow(dead_code, unreachable_pub)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use graft_client::fetch::FetchOptions;
use graft_client::workcopy::{WcConfig, WorkingCopy, clone};
use graft_client::{Endpoint, Remote, Sparse};
use graft_engine::{Engine, EngineConfig};
use graft_lsp::{Ended, LspServer};
use graft_model::{
    Author, AuthorKind, BlobId, Change, ChangesetId, EntryKind, Metadata, ObjectId, ObjectKind,
    RepoPath, Timestamp,
};
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{
    AdminApi, ApiError, CallContext, Frame, ObjectApi, Principal, PrincipalKind, RepoApi,
    WorkspaceApi,
};
use graft_store::memory::MemoryStore;
use graft_testkit::clock::FakeClock;
use graft_testkit::server::TestServer;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::task::JoinHandle;

pub const REPO: &str = "r";
const NOW_MS: i64 = 1_800_000_000_000;

pub fn admin() -> CallContext {
    CallContext {
        principal: Principal {
            subject: "lsp-test".into(),
            kind: PrincipalKind::Human,
            session: None,
            work_item: None,
            scopes: vec![format!("repo:{REPO}:admin")],
        },
        request_id: "lsp-test".into(),
        deadline_ms: None,
    }
}

pub fn author(name: &str, kind: AuthorKind) -> Author {
    Author {
        name: name.into(),
        email: format!("{}@example.com", name.to_lowercase()),
        kind,
    }
}

/// The engine with a fake PR bridge for land.
pub struct Rig {
    engine: Engine<MemoryStore, MemoryStore>,
    prs: Mutex<Vec<ChangesetId>>,
}

impl Rig {
    pub async fn new() -> Arc<Self> {
        let store = Arc::new(MemoryStore::new());
        let engine = Engine::new(
            store.clone(),
            store,
            Arc::new(FakeClock::at(NOW_MS)),
            EngineConfig::default(),
        );
        engine
            .create_repo(&admin(), REPO.into())
            .await
            .expect("repo");
        Arc::new(Self {
            engine,
            prs: Mutex::new(Vec::new()),
        })
    }

    pub async fn tip(&self) -> Option<ChangesetId> {
        self.engine
            .resolve_ref(
                &admin(),
                ResolveRef {
                    repo: REPO.into(),
                    name: "main".into(),
                },
            )
            .await
            .ok()
            .map(|r| r.target)
    }

    /// Lands `files` (path → text) on `main` as one changeset by `author`.
    pub async fn land_on_main(
        &self,
        files: &[(&str, &str)],
        author: Author,
        metadata: &[(&str, &str)],
        message: &str,
    ) -> ChangesetId {
        let tip = self.tip().await;
        let changes = files
            .iter()
            .map(|(path, text)| Change::Put {
                path: RepoPath::new(path).unwrap(),
                kind: EntryKind::File,
                id: BlobId::hash(text.as_bytes()).0,
            })
            .collect();
        let blobs = files
            .iter()
            .map(|(_, text)| Frame::encode(ObjectKind::Blob, text.as_bytes()))
            .collect();
        let at = Timestamp {
            ms: NOW_MS - 3_600_000 + self.prs.lock().unwrap().len() as i64,
            offset_min: 0,
        };
        let created = self
            .engine
            .create_changeset(
                &admin(),
                CreateChangeset {
                    repo: REPO.into(),
                    parents: tip.into_iter().collect(),
                    changes,
                    author,
                    authored_at: at,
                    message: message.into(),
                    metadata: metadata
                        .iter()
                        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                        .collect::<Metadata>(),
                    inline_blobs: blobs,
                    workspace: None,
                },
            )
            .await
            .expect("create");
        self.engine
            .update_ref(
                &admin(),
                UpdateRef {
                    repo: REPO.into(),
                    name: "main".into(),
                    expected: tip,
                    new: created.id,
                    kind: RefKind::Line,
                },
            )
            .await
            .expect("move main");
        created.id
    }

    pub fn prs(&self) -> Vec<ChangesetId> {
        self.prs.lock().unwrap().clone()
    }
}

fn fake() -> ApiError {
    ApiError::Rejected {
        policy: "fake".into(),
        detail: "not modelled by the rig".into(),
    }
}

impl ObjectApi for Rig {
    async fn get_objects(&self, _: &CallContext, req: GetObjects) -> ApiResult<Objects> {
        self.engine.get_objects(&admin(), req).await
    }
    async fn has_objects(&self, _: &CallContext, req: HasObjects) -> ApiResult<Vec<ObjectId>> {
        self.engine.has_objects(&admin(), req).await
    }
    async fn put_objects(&self, _: &CallContext, req: PutObjects) -> ApiResult<Vec<ObjectId>> {
        self.engine.put_objects(&admin(), req).await
    }
}

impl RepoApi for Rig {
    async fn create_changeset(&self, _: &CallContext, req: CreateChangeset) -> ApiResult<Created> {
        self.engine.create_changeset(&admin(), req).await
    }
    async fn get_changesets(&self, _: &CallContext, req: GetChangesets) -> ApiResult<Changesets> {
        self.engine.get_changesets(&admin(), req).await
    }
    async fn resolve_ref(&self, _: &CallContext, req: ResolveRef) -> ApiResult<RefView> {
        self.engine.resolve_ref(&admin(), req).await
    }
    async fn list_refs(&self, _: &CallContext, req: ListRefs) -> ApiResult<Vec<RefView>> {
        self.engine.list_refs(&admin(), req).await
    }
    async fn update_ref(&self, _: &CallContext, req: UpdateRef) -> ApiResult<RefView> {
        self.engine.update_ref(&admin(), req).await
    }
    async fn stat(&self, _: &CallContext, req: PathAt) -> ApiResult<EntryView> {
        self.engine.stat(&admin(), req).await
    }
    async fn list_tree(&self, _: &CallContext, req: ListTree) -> ApiResult<Entries> {
        self.engine.list_tree(&admin(), req).await
    }
    async fn read_file(&self, _: &CallContext, req: PathAt) -> ApiResult<FileContent> {
        self.engine.read_file(&admin(), req).await
    }
    async fn log(&self, _: &CallContext, req: Log) -> ApiResult<History> {
        self.engine.log(&admin(), req).await
    }
    async fn is_ancestor(&self, _: &CallContext, req: Pair) -> ApiResult<bool> {
        self.engine.is_ancestor(&admin(), req).await
    }
    async fn merge_base(&self, _: &CallContext, req: Pair) -> ApiResult<Option<ChangesetId>> {
        self.engine.merge_base(&admin(), req).await
    }
    async fn diff(&self, _: &CallContext, req: Diff) -> ApiResult<DiffResult> {
        self.engine.diff(&admin(), req).await
    }
    async fn blame(&self, _: &CallContext, req: Blame) -> ApiResult<BlameResult> {
        self.engine.blame(&admin(), req).await
    }
    async fn rebase(&self, _: &CallContext, req: Rebase) -> ApiResult<ChangesetId> {
        self.engine.rebase(&admin(), req).await
    }
    async fn revert(&self, _: &CallContext, _: Revert) -> ApiResult<ChangesetId> {
        Err(fake())
    }
    /// The PR bridge: the stack must sit on the line tip.
    async fn land(&self, _: &CallContext, req: Land) -> ApiResult<Landed> {
        let tip = self.tip().await;
        let head = self
            .engine
            .get_changesets(
                &admin(),
                GetChangesets {
                    repo: REPO.into(),
                    ids: vec![req.head],
                },
            )
            .await?
            .found
            .into_iter()
            .next()
            .ok_or_else(fake)?;
        if head.changeset.parents.first().copied() != tip {
            return Err(ApiError::Rejected {
                policy: "pr-bridge".into(),
                detail: "the stack is not on the line tip; sync first".into(),
            });
        }
        let mut prs = self.prs.lock().unwrap();
        let n = match prs.iter().position(|h| *h == req.head) {
            Some(i) => i + 1,
            None => {
                prs.push(req.head);
                prs.len()
            }
        };
        Ok(Landed::Queued {
            pr_url: format!("https://github.com/example/r/pull/{n}"),
        })
    }
    async fn attest(&self, _: &CallContext, _: Attest) -> ApiResult<()> {
        Err(fake())
    }
    async fn watch(&self, _: &CallContext, req: Watch) -> ApiResult<JournalPage> {
        self.engine.watch(&admin(), req).await
    }
}

impl WorkspaceApi for Rig {
    async fn open_workspace(
        &self,
        _: &CallContext,
        req: OpenWorkspace,
    ) -> ApiResult<WorkspaceView> {
        self.engine.open_workspace(&admin(), req).await
    }
    async fn get_workspace(&self, _: &CallContext, req: WorkspaceRef) -> ApiResult<WorkspaceView> {
        self.engine.get_workspace(&admin(), req).await
    }
    async fn list_workspaces(
        &self,
        _: &CallContext,
        repo: String,
        page: Page,
    ) -> ApiResult<Vec<WorkspaceView>> {
        self.engine.list_workspaces(&admin(), repo, page).await
    }
    async fn renew_lease(
        &self,
        _: &CallContext,
        req: WorkspaceRef,
        lease_ms: u64,
    ) -> ApiResult<WorkspaceView> {
        self.engine.renew_lease(&admin(), req, lease_ms).await
    }
    async fn set_head(&self, _: &CallContext, req: SetHead) -> ApiResult<WorkspaceView> {
        self.engine.set_head(&admin(), req).await
    }
    async fn sync(
        &self,
        _: &CallContext,
        req: WorkspaceRef,
        onto: Option<ChangesetId>,
    ) -> ApiResult<Synced> {
        self.engine.sync(&admin(), req, onto).await
    }
    async fn stack(
        &self,
        _: &CallContext,
        _: WorkspaceRef,
        _: StackOp,
    ) -> ApiResult<WorkspaceView> {
        Err(fake())
    }
    async fn close_workspace(
        &self,
        _: &CallContext,
        req: WorkspaceRef,
        keep_drafts: bool,
    ) -> ApiResult<()> {
        self.engine
            .close_workspace(&admin(), req, keep_drafts)
            .await
    }
}

/// The standard history on `main`:
///
/// - `c1` by Ada (human): `README.md`, and `src/lib.rs` with two lines;
/// - `c2` by agent-7 (agent, work item WI-2257): changes line 1 of
///   `src/lib.rs`.
pub struct Fixture {
    pub c1: ChangesetId,
    pub c2: ChangesetId,
}

pub const LIB_C1: &str = "fn a() {}\nfn b() {}\n";
pub const LIB_C2: &str = "fn a() { 1 }\nfn b() {}\n";

pub async fn history(rig: &Rig) -> Fixture {
    let c1 = rig
        .land_on_main(
            &[("README.md", "# r\n"), ("src/lib.rs", LIB_C1)],
            author("Ada", AuthorKind::Human),
            &[],
            "start the repository\n\nwith a body",
        )
        .await;
    let c2 = rig
        .land_on_main(
            &[("src/lib.rs", LIB_C2)],
            author("agent-7", AuthorKind::Agent),
            &[("vogt.work_item", "WI-2257"), ("vogt.session", "ses_test")],
            "make a return 1",
        )
        .await;
    Fixture { c1, c2 }
}

/// A served rig, a working copy of `main` holding `src/` (not the root
/// files), and a scripted client talking to an LSP server over it.
pub struct Env {
    pub rig: Arc<Rig>,
    pub server: TestServer,
    pub dir: tempfile::TempDir,
    pub root: std::path::PathBuf,
    pub history: Fixture,
    pub client: Client,
}

impl Env {
    pub async fn new() -> Env {
        let rig = Rig::new().await;
        let history = history(&rig).await;
        let server = TestServer::tcp_workspaces(rig.clone()).await;
        let remote = remote(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("wc");
        let (wc, _) = clone(
            &remote,
            &admin(),
            &root,
            WcConfig {
                endpoint: Some(server.url().to_owned()),
                repo: REPO.into(),
                workspace: None,
            },
            "main",
            None,
            Sparse::new([RepoPath::new("src").unwrap()], false),
            &FetchOptions::default(),
        )
        .await
        .expect("clone");
        let lsp =
            LspServer::new(remote, REPO, Some(wc)).with_author(author("Lin", AuthorKind::Human));
        let client = Client::start(lsp);
        Env {
            rig,
            server,
            dir,
            root,
            history,
            client,
        }
    }

    /// The `file:` URI of a working-copy path.
    pub fn uri(&self, path: &str) -> String {
        graft_lsp::file_uri(&self.root.join(path))
    }

    pub fn write(&self, path: &str, text: &str) {
        let file = self.root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }

    pub fn working_copy(&self) -> WorkingCopy {
        WorkingCopy::open(&self.root).unwrap()
    }

    /// `initialize` with the working copy as the root, then `initialized`.
    pub async fn initialize(&mut self) -> Value {
        let root = graft_lsp::file_uri(&self.root);
        let out = self
            .client
            .request(
                "initialize",
                json!({
                    "processId": null,
                    "rootUri": root,
                    "capabilities": {"workspace": {"codeLens": {"refreshSupport": true}}},
                }),
            )
            .await;
        self.client.notify("initialized", json!({})).await;
        out
    }

    pub async fn open(&mut self, uri: &str) -> Vec<Value> {
        self.client
            .notify(
                "textDocument/didOpen",
                json!({"textDocument": {"uri": uri, "languageId": "rust", "version": 1, "text": ""}}),
            )
            .await;
        self.client.diagnostics_for(uri).await
    }

    pub async fn hover(&mut self, uri: &str, line: u32) -> Value {
        self.client
            .request(
                "textDocument/hover",
                json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": 0}}),
            )
            .await
    }

    pub async fn lenses(&mut self, uri: &str) -> Vec<String> {
        let out = self
            .client
            .request(
                "textDocument/codeLens",
                json!({"textDocument": {"uri": uri}}),
            )
            .await;
        out["result"]
            .as_array()
            .expect("lenses")
            .iter()
            .map(|l| l["command"]["title"].as_str().unwrap().to_owned())
            .collect()
    }

    pub async fn command(&mut self, command: &str, arguments: Value) -> Value {
        self.client
            .request(
                "workspace/executeCommand",
                json!({"command": command, "arguments": arguments}),
            )
            .await
    }
}

pub async fn remote(server: &TestServer) -> Remote {
    Remote::connect(Endpoint::parse(server.url()))
        .await
        .expect("connect")
}

/// How long the client waits for one message.
const WAIT: Duration = Duration::from_secs(20);

/// A scripted LSP client over an in-memory pipe.
pub struct Client {
    writer: tokio::io::WriteHalf<DuplexStream>,
    reader: BufReader<tokio::io::ReadHalf<DuplexStream>>,
    next_id: i64,
    /// Notifications and server requests read while waiting for responses.
    pub seen: Vec<Value>,
    pub task: Option<JoinHandle<std::io::Result<Ended>>>,
}

impl Client {
    pub fn start(server: LspServer<Remote>) -> Client {
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        let (server_read, server_write) = tokio::io::split(server_io);
        let task = tokio::spawn(server.serve(server_read, server_write));
        let (read, writer) = tokio::io::split(client_io);
        Client {
            writer,
            reader: BufReader::new(read),
            next_id: 1,
            seen: Vec::new(),
            task: Some(task),
        }
    }

    pub async fn send_raw(&mut self, body: &[u8]) {
        self.writer
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await
            .unwrap();
        self.writer.write_all(body).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    pub async fn send(&mut self, msg: &Value) {
        self.send_raw(&serde_json::to_vec(msg).unwrap()).await;
    }

    /// Reads one message from the server.
    pub async fn read(&mut self) -> Value {
        tokio::time::timeout(WAIT, async {
            let mut length = None;
            loop {
                let mut line = String::new();
                let n = self.reader.read_line(&mut line).await.unwrap();
                assert!(n > 0, "the server closed its output");
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                let (name, value) = line.split_once(':').expect("a header");
                assert_eq!(name, "Content-Length");
                length = Some(value.trim().parse::<usize>().unwrap());
            }
            let mut body = vec![0; length.expect("Content-Length")];
            self.reader.read_exact(&mut body).await.unwrap();
            serde_json::from_slice(&body).expect("JSON from the server")
        })
        .await
        .expect("a message from the server in time")
    }

    /// Sends a request and returns the whole response message, keeping the
    /// messages that come before it in `seen`.
    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        self.response(json!(id)).await
    }

    /// Reads until the response to `id`.
    pub async fn response(&mut self, id: Value) -> Value {
        loop {
            let msg = self.read().await;
            if msg.get("method").is_none() && msg["id"] == id {
                assert_eq!(msg["jsonrpc"], "2.0");
                return msg;
            }
            self.seen.push(msg);
        }
    }

    pub async fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    /// The next diagnostics published for `uri` (from `seen`, else read).
    pub async fn diagnostics_for(&mut self, uri: &str) -> Vec<Value> {
        let is_it = |m: &Value| {
            m["method"] == "textDocument/publishDiagnostics" && m["params"]["uri"] == uri
        };
        if let Some(i) = self.seen.iter().position(is_it) {
            let m = self.seen.remove(i);
            return m["params"]["diagnostics"].as_array().unwrap().clone();
        }
        loop {
            let m = self.read().await;
            if is_it(&m) {
                return m["params"]["diagnostics"].as_array().unwrap().clone();
            }
            self.seen.push(m);
        }
    }

    /// Drops what was seen and returns it.
    pub fn take_seen(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.seen)
    }

    /// Waits for the server to stop.
    pub async fn ended(&mut self) -> Ended {
        let task = self.task.take().expect("running");
        tokio::time::timeout(WAIT, task)
            .await
            .expect("the server stops in time")
            .expect("the server task")
            .expect("the server's I/O")
    }
}
