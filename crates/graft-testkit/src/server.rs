//! An in-process gRPC server on a loopback socket (work item G1.11).
//!
//! [`TestServer`] serves any `ObjectApi + RepoApi` over graft-proto's
//! generated `ObjectService` and `RepoService`, on `127.0.0.1` or a Unix
//! socket, binding the protocol version and compressing the way the real
//! server does. It records every RPC with the ids it named, so tests can
//! count requests, and it can drop every live connection mid-stream (fault
//! injection for resume tests).
//!
//! Until graft-engine serves the API, [`SnapshotRepo`] is the repository to
//! put behind it: [`FakeApi`] for objects and changesets, plus lines and a
//! `Diff` computed with graft-model's id-pruned tree diff.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use futures_util::task::AtomicWaker;
use futures_util::{Stream, StreamExt};
use graft_model::{
    ApplyOptions, Author, AuthorKind, BlobId, Change, Changeset, ChangesetId, EntryKind, Metadata,
    ObjectId, ObjectKind, ObjectSource, RepoPath, SourceError, Timestamp, Tree, TreeId,
    apply_changes_with, diff_trees_after,
};
use graft_proto::api::{AdminApi, ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::error::ApiResult;
use graft_proto::pb;
use graft_proto::pb::object_service_server::{ObjectService, ObjectServiceServer};
use graft_proto::pb::repo_service_server::{RepoService, RepoServiceServer};
use graft_proto::pb::review_service_server::{ReviewService, ReviewServiceServer};
use graft_proto::pb::workspace_service_server::WorkspaceServiceServer;
use graft_proto::types::*;
use graft_proto::{
    ApiError, CallContext, Frame, PROTOCOL_VERSION, Principal, PrincipalKind, ProtocolVersion,
    VERSION_HEADER,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::task::JoinHandle;
use tonic::codec::CompressionEncoding;
use tonic::metadata::MetadataValue;
use tonic::transport::server::Router;
use tonic::{Request, Response, Status};

use crate::fake_api::{FakeApi, admin_cx};

#[path = "snapshot_workspaces.rs"]
mod workspaces;

/// Frames per `GetObjects` response page.
pub const PAGE_FRAMES: usize = 16;

/// The largest message the server accepts or sends.
const MAX_MESSAGE_BYTES: usize = 72 * 1024 * 1024;

/// One recorded RPC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rpc {
    /// `GetObjects` for these ids.
    GetObjects(Vec<ObjectId>),
    /// `HasObjects` for these ids.
    HasObjects(Vec<ObjectId>),
    /// `PutObjects` with this many blobs.
    PutObjects(usize),
    /// A `RepoService` call, by method name.
    Repo(&'static str),
    /// A `WorkspaceService` call, by method name.
    Workspace(&'static str),
}

/// A served API on a loopback socket. Stops when dropped.
pub struct TestServer {
    url: String,
    socket: Option<PathBuf>,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TestServer {
    /// Serves `api` on `127.0.0.1` at a free port.
    pub async fn tcp<A: ObjectApi + RepoApi + 'static>(api: Arc<A>) -> TestServer {
        Self::tcp_with(|shared, incoming| spawn(router(api, shared), incoming)).await
    }

    /// [`TestServer::tcp`], also serving `WorkspaceService`.
    pub async fn tcp_workspaces<A: ObjectApi + RepoApi + WorkspaceApi + 'static>(
        api: Arc<A>,
    ) -> TestServer {
        Self::tcp_with(|shared, incoming| spawn(workspace_router(api, shared), incoming)).await
    }

    async fn tcp_with<F>(start: F) -> TestServer
    where
        F: FnOnce(Arc<Shared>, TcpIncoming) -> JoinHandle<()>,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr: SocketAddr = listener.local_addr().expect("local addr");
        let shared = Arc::new(Shared::default());
        let wrap = shared.clone();
        let incoming: TcpIncoming = Box::pin(
            tokio_stream::wrappers::TcpListenerStream::new(listener)
                .map(move |s| s.map(|s| wrap.wrap(s))),
        );
        let task = start(shared.clone(), incoming);
        TestServer {
            url: format!("http://{addr}"),
            socket: None,
            shared,
            task,
        }
    }

    /// Serves `api` on a Unix socket at `path`.
    pub async fn unix<A: ObjectApi + RepoApi + 'static>(api: Arc<A>, path: &Path) -> TestServer {
        let listener = tokio::net::UnixListener::bind(path).expect("bind unix socket");
        let shared = Arc::new(Shared::default());
        let wrap = shared.clone();
        let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener)
            .map(move |s| s.map(|s| wrap.wrap(s)));
        let task = spawn(router(api, shared.clone()), incoming);
        TestServer {
            url: format!("unix:{}", path.display()),
            socket: Some(path.to_owned()),
            shared,
            task,
        }
    }

    /// `http://127.0.0.1:<port>`, or `unix:<path>`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The Unix socket path, when serving on one.
    pub fn socket_path(&self) -> Option<&Path> {
        self.socket.as_deref()
    }

    /// Every RPC so far, oldest first.
    pub fn calls(&self) -> Vec<Rpc> {
        self.shared.calls.lock().expect("calls").clone()
    }

    /// Returns and clears the recorded RPCs.
    pub fn take_calls(&self) -> Vec<Rpc> {
        std::mem::take(&mut *self.shared.calls.lock().expect("calls"))
    }

    /// Every id named by a `GetObjects` call so far, in order.
    pub fn requested_ids(&self) -> Vec<ObjectId> {
        self.calls()
            .into_iter()
            .flat_map(|c| match c {
                Rpc::GetObjects(ids) => ids,
                _ => Vec::new(),
            })
            .collect()
    }

    /// The number of `GetObjects` calls so far.
    pub fn get_objects_calls(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| matches!(c, Rpc::GetObjects(_)))
            .count()
    }

    /// Once `frames` more frames have been streamed, drops every live
    /// connection in the middle of the next `GetObjects` page. One-shot;
    /// later connections are served normally.
    pub fn drop_connections_after(&self, frames: u64) {
        let at = self.shared.frames_sent.load(Ordering::SeqCst) + frames;
        *self.shared.drop_at.lock().expect("drop_at") = Some(at);
    }

    /// How many times connections were dropped by fault injection.
    pub fn drops(&self) -> u64 {
        self.shared.drops.load(Ordering::SeqCst)
    }

    /// Speaks `version` from now on (for compatibility tests).
    pub fn set_protocol_version(&self, version: ProtocolVersion) {
        *self.shared.protocol.lock().expect("protocol") = version;
    }

    /// Whether responses carry the protocol-version header (on by default;
    /// off imitates a peer that is not graft, or one that strips it).
    pub fn advertise_version(&self, on: bool) {
        self.shared.advertise.store(on, Ordering::SeqCst);
    }
}

/// The accepted TCP connections, wrapped for fault injection.
type TcpIncoming =
    Pin<Box<dyn Stream<Item = io::Result<Conn<tokio::net::TcpStream>>> + Send + 'static>>;

fn spawn<I, IO>(router: Router, incoming: I) -> JoinHandle<()>
where
    I: Stream<Item = io::Result<IO>> + Send + 'static,
    IO: AsyncRead + AsyncWrite + tonic::transport::server::Connected + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let _ = router.serve_with_incoming(incoming).await;
    })
}

/// The object and repository services, plus the workspace service.
fn workspace_router<A>(api: Arc<A>, shared: Arc<Shared>) -> Router
where
    A: ObjectApi + RepoApi + WorkspaceApi + 'static,
{
    let svc = Arc::new(Svc { api, shared });
    let workspaces = WorkspaceServiceServer::from_arc(svc.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .send_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
    let review = ReviewServiceServer::from_arc(svc.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .send_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
    services(svc).add_service(workspaces).add_service(review)
}

fn router<A: ObjectApi + RepoApi + 'static>(api: Arc<A>, shared: Arc<Shared>) -> Router {
    services(Arc::new(Svc { api, shared }))
}

fn services<A: ObjectApi + RepoApi + 'static>(svc: Arc<Svc<A>>) -> Router {
    let objects = ObjectServiceServer::from_arc(svc.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
    let repo = RepoServiceServer::from_arc(svc)
        .accept_compressed(CompressionEncoding::Gzip)
        .send_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
    tonic::transport::Server::builder()
        .add_service(objects)
        .add_service(repo)
}

struct Shared {
    calls: Mutex<Vec<Rpc>>,
    frames_sent: AtomicU64,
    drop_at: Mutex<Option<u64>>,
    drops: AtomicU64,
    conns: Mutex<Vec<Weak<ConnState>>>,
    protocol: Mutex<ProtocolVersion>,
    advertise: AtomicBool,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            calls: Mutex::default(),
            frames_sent: AtomicU64::new(0),
            drop_at: Mutex::default(),
            drops: AtomicU64::new(0),
            conns: Mutex::default(),
            protocol: Mutex::new(PROTOCOL_VERSION),
            advertise: AtomicBool::new(true),
        }
    }
}

impl Shared {
    fn record(&self, rpc: Rpc) {
        self.calls.lock().expect("calls").push(rpc);
    }

    fn wrap<T>(&self, io: T) -> Conn<T> {
        let state = Arc::new(ConnState::default());
        let mut conns = self.conns.lock().expect("conns");
        conns.retain(|c| c.strong_count() > 0);
        conns.push(Arc::downgrade(&state));
        Conn { io, state }
    }

    /// Counts a page of `n` frames; true when the fault fires on it.
    fn page_drops(&self, n: usize) -> bool {
        let sent = self.frames_sent.fetch_add(n as u64, Ordering::SeqCst) + n as u64;
        let mut at = self.drop_at.lock().expect("drop_at");
        if at.is_some_and(|limit| sent > limit) {
            *at = None;
            return true;
        }
        false
    }

    fn drop_all(&self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        for c in self.conns.lock().expect("conns").iter() {
            if let Some(c) = c.upgrade() {
                c.killed.store(true, Ordering::SeqCst);
                c.waker.wake();
            }
        }
    }
}

#[derive(Default)]
struct ConnState {
    killed: AtomicBool,
    waker: AtomicWaker,
}

/// A connection that fault injection can kill: once killed, every read and
/// write fails, so the server tears the connection down.
struct Conn<T> {
    io: T,
    state: Arc<ConnState>,
}

impl<T> Conn<T> {
    fn check(&self, cx: &Context<'_>) -> Option<io::Error> {
        self.state.waker.register(cx.waker());
        self.state
            .killed
            .load(Ordering::SeqCst)
            .then(|| io::ErrorKind::ConnectionReset.into())
    }
}

impl<T> tonic::transport::server::Connected for Conn<T> {
    type ConnectInfo = ();
    fn connect_info(&self) {}
}

impl<T: AsyncRead + Unpin> AsyncRead for Conn<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(e) = this.check(cx) {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut this.io).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Conn<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Some(e) = this.check(cx) {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut this.io).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(e) = this.check(cx) {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut this.io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

/// The gRPC services over an API.
struct Svc<A> {
    api: Arc<A>,
    shared: Arc<Shared>,
}

impl<A> Svc<A> {
    /// Binds the client's protocol version and builds the call context.
    fn bind<T>(&self, req: &Request<T>) -> Result<CallContext, Status> {
        let md = req.metadata();
        let header = md.get(VERSION_HEADER).and_then(|v| v.to_str().ok());
        self.shared
            .protocol
            .lock()
            .expect("protocol")
            .accept(header)?;
        let request_id = md
            .get("graft-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        Ok(CallContext {
            principal: Principal {
                subject: "test-client".to_owned(),
                kind: PrincipalKind::Service,
                session: None,
                work_item: None,
                scopes: vec!["admin".to_owned()],
            },
            request_id,
            deadline_ms: None,
        })
    }

    /// A response advertising the server's protocol version.
    fn reply<T>(&self, msg: T) -> Response<T> {
        let version = self.shared.protocol.lock().expect("protocol").to_string();
        let mut r = Response::new(msg);
        if !self.shared.advertise.load(Ordering::SeqCst) {
            return r;
        }
        r.metadata_mut().insert(
            VERSION_HEADER,
            MetadataValue::try_from(version).expect("version is ASCII"),
        );
        r
    }
}

/// Converts a wire request, refusing it as the server's boundary does.
fn conv<T, U>(t: T) -> Result<U, Status>
where
    U: TryFrom<T>,
    U::Error: std::fmt::Display,
{
    U::try_from(t).map_err(|e| Status::invalid_argument(e.to_string()))
}

/// Splits an answer into pages of at most [`PAGE_FRAMES`] frames; the last
/// page carries `missing` and `deferred`.
fn pages(objects: Objects) -> Vec<pb::Objects> {
    let mut found = objects.found;
    let mut out = Vec::new();
    while found.len() > PAGE_FRAMES {
        let rest = found.split_off(PAGE_FRAMES);
        out.push(pb::Objects::from(Objects {
            found: std::mem::replace(&mut found, rest),
            ..Objects::default()
        }));
    }
    out.push(pb::Objects::from(Objects {
        found,
        missing: objects.missing,
        deferred: objects.deferred,
    }));
    out
}

type ObjectPages = Pin<Box<dyn Stream<Item = Result<pb::Objects, Status>> + Send>>;
type JournalPages = Pin<Box<dyn Stream<Item = Result<pb::JournalPage, Status>> + Send>>;

#[tonic::async_trait]
impl<A: ObjectApi + RepoApi + 'static> ObjectService for Svc<A> {
    type GetObjectsStream = ObjectPages;

    async fn get_objects(
        &self,
        req: Request<pb::GetObjectsRequest>,
    ) -> Result<Response<ObjectPages>, Status> {
        let cx = self.bind(&req)?;
        let req: GetObjects = conv(req.into_inner())?;
        self.shared.record(Rpc::GetObjects(req.ids.clone()));
        let objects = self.api.get_objects(&cx, req).await?;
        let shared = self.shared.clone();
        let stream = futures_util::stream::iter(pages(objects)).map(move |page| {
            if shared.page_drops(page.found.len()) {
                shared.drop_all();
                return Err(Status::unavailable("connection dropped by fault injection"));
            }
            Ok(page)
        });
        Ok(self.reply(Box::pin(stream)))
    }

    async fn has_objects(
        &self,
        req: Request<pb::HasObjectsRequest>,
    ) -> Result<Response<pb::ObjectIds>, Status> {
        let cx = self.bind(&req)?;
        let req: HasObjects = conv(req.into_inner())?;
        self.shared.record(Rpc::HasObjects(req.ids.clone()));
        let missing = self.api.has_objects(&cx, req).await?;
        Ok(self.reply(pb::ObjectIds::from(missing)))
    }

    async fn put_objects(
        &self,
        req: Request<tonic::Streaming<pb::PutObjectsRequest>>,
    ) -> Result<Response<pb::ObjectIds>, Status> {
        let cx = self.bind(&req)?;
        let mut stream = req.into_inner();
        let mut all = PutObjects::default();
        while let Some(msg) = stream.message().await? {
            let msg: PutObjects = conv(msg)?;
            all.repo = msg.repo;
            all.blobs.extend(msg.blobs);
        }
        self.shared.record(Rpc::PutObjects(all.blobs.len()));
        let ids = self.api.put_objects(&cx, all).await?;
        Ok(self.reply(pb::ObjectIds::from(ids)))
    }
}

/// One unary repository call: bind, convert, record, call, reply.
macro_rules! unary {
    ($self:ident, $req:ident, $name:literal, $rust:ty, $method:ident, |$out:ident| $wire:expr) => {{
        let cx = $self.bind(&$req)?;
        let r: $rust = conv($req.into_inner())?;
        $self.shared.record(Rpc::Repo($name));
        let $out = $self.api.$method(&cx, r).await?;
        Ok($self.reply($wire))
    }};
}

#[tonic::async_trait]
impl<A: ObjectApi + RepoApi + 'static> RepoService for Svc<A> {
    type WatchStream = JournalPages;

    async fn create_changeset(
        &self,
        req: Request<pb::CreateChangesetRequest>,
    ) -> Result<Response<pb::Created>, Status> {
        unary!(
            self,
            req,
            "CreateChangeset",
            CreateChangeset,
            create_changeset,
            |r| pb::Created::from(r)
        )
    }

    async fn get_changesets(
        &self,
        req: Request<pb::GetChangesetsRequest>,
    ) -> Result<Response<pb::Changesets>, Status> {
        unary!(
            self,
            req,
            "GetChangesets",
            GetChangesets,
            get_changesets,
            |r| pb::Changesets::from(r)
        )
    }

    async fn resolve_ref(
        &self,
        req: Request<pb::ResolveRefRequest>,
    ) -> Result<Response<pb::RefView>, Status> {
        unary!(self, req, "ResolveRef", ResolveRef, resolve_ref, |r| {
            pb::RefView::from(r)
        })
    }

    async fn list_refs(
        &self,
        req: Request<pb::ListRefsRequest>,
    ) -> Result<Response<pb::RefViews>, Status> {
        unary!(self, req, "ListRefs", ListRefs, list_refs, |r| {
            pb::RefViews::from(r)
        })
    }

    async fn update_ref(
        &self,
        req: Request<pb::UpdateRefRequest>,
    ) -> Result<Response<pb::RefView>, Status> {
        unary!(self, req, "UpdateRef", UpdateRef, update_ref, |r| {
            pb::RefView::from(r)
        })
    }

    async fn create_line(
        &self,
        req: Request<pb::CreateLineRequest>,
    ) -> Result<Response<pb::RefView>, Status> {
        unary!(self, req, "CreateLine", CreateLine, create_line, |r| {
            pb::RefView::from(r)
        })
    }

    async fn line_merge(
        &self,
        req: Request<pb::LineMergeRequest>,
    ) -> Result<Response<pb::LineMergeOutcome>, Status> {
        unary!(self, req, "LineMerge", LineMerge, line_merge, |r| {
            pb::LineMergeOutcome::from(r)
        })
    }

    async fn line_merges(
        &self,
        req: Request<pb::LineMergesRequest>,
    ) -> Result<Response<pb::LineMergePage>, Status> {
        unary!(self, req, "LineMerges", LineMerges, line_merges, |r| {
            pb::LineMergePage::from(r)
        })
    }

    async fn stat(&self, req: Request<pb::PathAt>) -> Result<Response<pb::EntryView>, Status> {
        unary!(self, req, "Stat", PathAt, stat, |r| pb::EntryView::from(r))
    }

    async fn list_tree(
        &self,
        req: Request<pb::ListTreeRequest>,
    ) -> Result<Response<pb::Entries>, Status> {
        unary!(self, req, "ListTree", ListTree, list_tree, |r| {
            pb::Entries::from(r)
        })
    }

    async fn read_file(
        &self,
        req: Request<pb::PathAt>,
    ) -> Result<Response<pb::FileContent>, Status> {
        unary!(self, req, "ReadFile", PathAt, read_file, |r| {
            pb::FileContent::from(r)
        })
    }

    async fn log(&self, req: Request<pb::LogRequest>) -> Result<Response<pb::History>, Status> {
        unary!(self, req, "Log", Log, log, |r| pb::History::from(r))
    }

    async fn is_ancestor(&self, req: Request<pb::Pair>) -> Result<Response<pb::Bool>, Status> {
        unary!(self, req, "IsAncestor", Pair, is_ancestor, |r| pb::Bool {
            value: r
        })
    }

    async fn merge_base(
        &self,
        req: Request<pb::Pair>,
    ) -> Result<Response<pb::OptionalChangeset>, Status> {
        unary!(self, req, "MergeBase", Pair, merge_base, |r| {
            pb::OptionalChangeset {
                id: r.map(|id| id.0.0.to_vec()),
            }
        })
    }

    async fn diff(
        &self,
        req: Request<pb::DiffRequest>,
    ) -> Result<Response<pb::DiffResult>, Status> {
        unary!(self, req, "Diff", Diff, diff, |r| pb::DiffResult::from(r))
    }

    async fn blame(
        &self,
        req: Request<pb::BlameRequest>,
    ) -> Result<Response<pb::BlameResult>, Status> {
        unary!(self, req, "Blame", Blame, blame, |r| pb::BlameResult::from(
            r
        ))
    }

    async fn rebase(
        &self,
        req: Request<pb::RebaseRequest>,
    ) -> Result<Response<pb::ChangesetRef>, Status> {
        unary!(self, req, "Rebase", Rebase, rebase, |r| pb::ChangesetRef {
            id: r.0.0.to_vec()
        })
    }

    async fn revert(
        &self,
        req: Request<pb::RevertRequest>,
    ) -> Result<Response<pb::ChangesetRef>, Status> {
        unary!(self, req, "Revert", Revert, revert, |r| pb::ChangesetRef {
            id: r.0.0.to_vec()
        })
    }

    async fn land(&self, req: Request<pb::LandRequest>) -> Result<Response<pb::Landed>, Status> {
        unary!(self, req, "Land", Land, land, |r| pb::Landed::from(r))
    }

    async fn attest(&self, req: Request<pb::AttestRequest>) -> Result<Response<pb::Empty>, Status> {
        // Old clients leave kind and status unspecified; those still call
        // `attest`. A set v0.2 field is the only reason to call `attest_v2`,
        // because a backing API that never implemented it must keep working.
        let inner = req.get_ref();
        // A check whose status only restates `passed`, with no display
        // name, is the v0.1 call. Review, approval, pending, error and a
        // named by are v0.2.
        let kind = inner.kind;
        let status = inner.status;
        let check_shape =
            kind == pb::AttestKind::Unspecified as i32 || kind == pb::AttestKind::Check as i32;
        let status_restates_passed = status == pb::AttestStatus::Unspecified as i32
            || (inner.passed && status == pb::AttestStatus::Pass as i32)
            || (!inner.passed && status == pb::AttestStatus::Fail as i32);
        let v2 = !check_shape || !status_restates_passed || inner.by.is_some();
        if v2 {
            unary!(self, req, "Attest", AttestV2, attest_v2, |_r| pb::Empty {})
        } else {
            unary!(self, req, "Attest", Attest, attest, |_r| pb::Empty {})
        }
    }

    async fn watch(
        &self,
        req: Request<pb::WatchRequest>,
    ) -> Result<Response<JournalPages>, Status> {
        unary!(self, req, "Watch", Watch, watch, |r| {
            let page: Result<pb::JournalPage, Status> = Ok(pb::JournalPage::from(r));
            Box::pin(futures_util::stream::iter([page])) as JournalPages
        })
    }
}

#[tonic::async_trait]
impl<A: ObjectApi + RepoApi + 'static> ReviewService for Svc<A> {
    async fn get_attestations(
        &self,
        req: Request<pb::GetAttestationsRequest>,
    ) -> Result<Response<pb::AttestationPage>, Status> {
        unary!(
            self,
            req,
            "GetAttestations",
            GetAttestations,
            get_attestations,
            |r| pb::AttestationPage::from(r)
        )
    }

    async fn get_land_checks(
        &self,
        req: Request<pb::GetLandChecksRequest>,
    ) -> Result<Response<pb::LandCheckPage>, Status> {
        unary!(
            self,
            req,
            "GetLandChecks",
            GetLandChecks,
            get_land_checks,
            |r| pb::LandCheckPage::from(r)
        )
    }

    async fn review_queue(
        &self,
        req: Request<pb::ReviewQueueRequest>,
    ) -> Result<Response<pb::ReviewQueuePage>, Status> {
        unary!(self, req, "ReviewQueue", ReviewQueue, review_queue, |r| {
            pb::ReviewQueuePage::from(r)
        })
    }

    async fn get_line_config(
        &self,
        req: Request<pb::GetLineConfigRequest>,
    ) -> Result<Response<pb::LineConfig>, Status> {
        unary!(
            self,
            req,
            "GetLineConfig",
            GetLineConfig,
            get_line_config,
            |r| pb::LineConfig::from(r)
        )
    }
}

/// The workspace a wire request names, refused when absent.
fn ws_ref(w: Option<pb::WorkspaceRef>) -> Result<WorkspaceRef, Status> {
    w.map(Into::into)
        .ok_or_else(|| Status::invalid_argument("workspace: required"))
}

#[tonic::async_trait]
impl<A: ObjectApi + RepoApi + WorkspaceApi + 'static> pb::workspace_service_server::WorkspaceService
    for Svc<A>
{
    async fn open_workspace(
        &self,
        req: Request<pb::OpenWorkspaceRequest>,
    ) -> Result<Response<pb::WorkspaceView>, Status> {
        let cx = self.bind(&req)?;
        let r: OpenWorkspace = conv(req.into_inner())?;
        self.shared.record(Rpc::Workspace("OpenWorkspace"));
        let out = self.api.open_workspace(&cx, r).await?;
        Ok(self.reply(out.into()))
    }

    async fn get_workspace(
        &self,
        req: Request<pb::WorkspaceRef>,
    ) -> Result<Response<pb::WorkspaceView>, Status> {
        let cx = self.bind(&req)?;
        let r = WorkspaceRef::from(req.into_inner());
        self.shared.record(Rpc::Workspace("GetWorkspace"));
        let out = self.api.get_workspace(&cx, r).await?;
        Ok(self.reply(out.into()))
    }

    async fn list_workspaces(
        &self,
        req: Request<pb::ListWorkspacesRequest>,
    ) -> Result<Response<pb::WorkspaceViews>, Status> {
        let cx = self.bind(&req)?;
        let r = req.into_inner();
        self.shared.record(Rpc::Workspace("ListWorkspaces"));
        let page = r.page.map(Into::into).unwrap_or_default();
        let out = self.api.list_workspaces(&cx, r.repo, page).await?;
        Ok(self.reply(out.into()))
    }

    async fn renew_lease(
        &self,
        req: Request<pb::RenewLeaseRequest>,
    ) -> Result<Response<pb::WorkspaceView>, Status> {
        let cx = self.bind(&req)?;
        let r = req.into_inner();
        let ws = ws_ref(r.workspace)?;
        self.shared.record(Rpc::Workspace("RenewLease"));
        let out = self.api.renew_lease(&cx, ws, r.lease_ms).await?;
        Ok(self.reply(out.into()))
    }

    async fn set_head(
        &self,
        req: Request<pb::SetHeadRequest>,
    ) -> Result<Response<pb::WorkspaceView>, Status> {
        let cx = self.bind(&req)?;
        let r: SetHead = conv(req.into_inner())?;
        self.shared.record(Rpc::Workspace("SetHead"));
        let out = self.api.set_head(&cx, r).await?;
        Ok(self.reply(out.into()))
    }

    async fn get_conflicts(
        &self,
        req: Request<pb::GetConflictsRequest>,
    ) -> Result<Response<pb::Conflicts>, Status> {
        let cx = self.bind(&req)?;
        let r: GetConflicts = conv(req.into_inner())?;
        self.shared.record(Rpc::Workspace("GetConflicts"));
        let out = self.api.get_conflicts(&cx, r).await?;
        Ok(self.reply(out.into()))
    }

    async fn overlapping_drafts(
        &self,
        req: Request<pb::OverlappingDraftsRequest>,
    ) -> Result<Response<pb::DraftOverlap>, Status> {
        let cx = self.bind(&req)?;
        let r: OverlappingDrafts = conv(req.into_inner())?;
        self.shared.record(Rpc::Workspace("OverlappingDrafts"));
        let out = self.api.overlapping_drafts(&cx, r).await?;
        Ok(self.reply(out.into()))
    }

    async fn resolve(
        &self,
        req: Request<pb::ResolveRequest>,
    ) -> Result<Response<pb::Resolved>, Status> {
        let cx = self.bind(&req)?;
        let r: Resolve = conv(req.into_inner())?;
        self.shared.record(Rpc::Workspace("Resolve"));
        let out = self.api.resolve(&cx, r).await?;
        Ok(self.reply(out.into()))
    }

    async fn sync(&self, req: Request<pb::SyncRequest>) -> Result<Response<pb::Synced>, Status> {
        let cx = self.bind(&req)?;
        let r = req.into_inner();
        let ws = ws_ref(r.workspace)?;
        let onto = match r.onto {
            None => None,
            Some(b) => Some(ChangesetId(ObjectId(
                <[u8; 32]>::try_from(b.as_slice())
                    .map_err(|_| Status::invalid_argument("onto: id is not 32 bytes"))?,
            ))),
        };
        self.shared.record(Rpc::Workspace("Sync"));
        let out = self.api.sync(&cx, ws, onto).await?;
        Ok(self.reply(out.into()))
    }

    async fn stack(
        &self,
        req: Request<pb::StackRequest>,
    ) -> Result<Response<pb::WorkspaceView>, Status> {
        let cx = self.bind(&req)?;
        let r = req.into_inner();
        let ws = ws_ref(r.workspace)?;
        let op: StackOp = conv(r.op.ok_or_else(|| Status::invalid_argument("op: required"))?)?;
        self.shared.record(Rpc::Workspace("Stack"));
        let out = self.api.stack(&cx, ws, op).await?;
        Ok(self.reply(out.into()))
    }

    async fn close_workspace(
        &self,
        req: Request<pb::CloseWorkspaceRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        let cx = self.bind(&req)?;
        let r = req.into_inner();
        let ws = ws_ref(r.workspace)?;
        self.shared.record(Rpc::Workspace("CloseWorkspace"));
        self.api.close_workspace(&cx, ws, r.keep_drafts).await?;
        Ok(self.reply(pb::Empty {}))
    }
}

// ---- SnapshotRepo ---------------------------------------------------------

struct Line {
    tip: ChangesetId,
    version: u64,
    files: BTreeMap<String, (EntryKind, Vec<u8>)>,
}

/// A small in-memory repository to serve: [`FakeApi`] holds the objects and
/// changesets; this adds lines (`ResolveRef`, `ListRefs`, `CreateLine`), `Diff`, and a
/// model of workspaces, drafts and land (`CreateChangeset`, `Land` and the
/// `WorkspaceApi`; see the `snapshot_workspaces` module). Other repository
/// operations answer `Rejected { policy: "fake" }`. Like [`FakeApi`], it
/// ignores the repository name.
///
/// For hostile-server tests it can also serve unvalidated objects and
/// changesets ([`SnapshotRepo::insert_raw`]), a canned `Diff`
/// ([`SnapshotRepo::lie_about_diff`]), and unrequested objects in every
/// `GetObjects` answer ([`SnapshotRepo::pad_get_objects`]).
#[derive(Default)]
pub struct SnapshotRepo {
    api: FakeApi,
    lines: Mutex<BTreeMap<String, Line>>,
    raw_objects: Mutex<HashMap<ObjectId, Frame>>,
    raw_changesets: Mutex<HashMap<ChangesetId, Changeset>>,
    diff_lie: Mutex<Option<DiffResult>>,
    padding: AtomicUsize,
    records: Mutex<HashMap<ChangesetId, (DraftState, Option<ChangesetId>)>>,
    ws: Mutex<workspaces::Workspaces>,
    failures: Mutex<HashMap<&'static str, VecDeque<ApiError>>>,
}

/// Reads trees from a [`FakeApi`] through `GetObjects`.
struct FakeSource<'a>(&'a FakeApi);

impl ObjectSource for FakeSource<'_> {
    async fn tree(&self, id: TreeId) -> Result<Tree, SourceError> {
        let got = self
            .0
            .get_objects(
                &admin_cx(),
                GetObjects {
                    repo: String::new(),
                    ids: vec![id.0],
                },
            )
            .await
            .map_err(|e| SourceError::Backend(e.to_string()))?;
        let (_, frame) = got.found.first().ok_or(SourceError::Missing(id))?;
        let payload = frame
            .decode_verified(id.0)
            .map_err(|_| SourceError::Corrupt(id))?;
        Tree::decode(&payload).map_err(|_| SourceError::Malformed(id))
    }
}

fn empty_root() -> TreeId {
    Tree::default().id()
}

fn non_empty(root: TreeId) -> Option<TreeId> {
    (root != empty_root()).then_some(root)
}

fn rejected<T>() -> ApiResult<T> {
    Err(ApiError::Rejected {
        policy: "fake".to_owned(),
        detail: "not implemented by SnapshotRepo".to_owned(),
    })
}

impl SnapshotRepo {
    /// An empty repository.
    pub fn new() -> Self {
        Self::default()
    }

    /// The underlying fake.
    pub fn fake(&self) -> &FakeApi {
        &self.api
    }

    /// The tip of `line`.
    pub fn tip(&self, line: &str) -> Option<ChangesetId> {
        self.lines.lock().expect("lines").get(line).map(|l| l.tip)
    }

    /// A changeset's root tree.
    pub fn root(&self, id: ChangesetId) -> Option<TreeId> {
        if let Some(c) = self.raw_changesets.lock().expect("raw").get(&id) {
            return Some(c.root);
        }
        self.api.changeset(id).map(|c| c.root)
    }

    /// Serves `changeset` and `objects` as given, unvalidated, and moves
    /// `line` to the changeset (a hostile server). The line can't take
    /// [`SnapshotRepo::commit`] afterwards.
    pub fn insert_raw(&self, line: &str, changeset: Changeset, objects: Vec<(ObjectId, Frame)>) {
        let id = changeset.id();
        self.raw_objects.lock().expect("raw").extend(objects);
        self.raw_changesets
            .lock()
            .expect("raw")
            .insert(id, changeset);
        let mut lines = self.lines.lock().expect("lines");
        let version = lines.get(line).map_or(0, |l| l.version);
        lines.insert(
            line.to_owned(),
            Line {
                tip: id,
                version: version + 1,
                files: BTreeMap::new(),
            },
        );
    }

    /// Answers every `Diff` with `result` (a lying server), or truthfully
    /// again with `None`.
    pub fn lie_about_diff(&self, result: Option<DiffResult>) {
        *self.diff_lie.lock().expect("diff") = result;
    }

    /// Appends `n` objects nobody asked for to every `GetObjects` answer.
    pub fn pad_get_objects(&self, n: usize) {
        self.padding.store(n, Ordering::SeqCst);
    }

    /// Commits `files` (path → bytes, all regular files) as the next
    /// changeset on `line`: the changes against the line's current snapshot,
    /// with its tip as the parent. Moves the line and returns the id; with
    /// no changes, returns the current tip.
    pub async fn commit(&self, line: &str, files: &BTreeMap<String, Vec<u8>>) -> ChangesetId {
        let entries = files
            .iter()
            .map(|(p, b)| (p.clone(), (EntryKind::File, b.clone())))
            .collect();
        self.commit_entries(line, &entries).await
    }

    /// [`SnapshotRepo::commit`] with each entry's kind given (file, exec or
    /// symlink, whose bytes are the target).
    pub async fn commit_entries(
        &self,
        line: &str,
        files: &BTreeMap<String, (EntryKind, Vec<u8>)>,
    ) -> ChangesetId {
        let (parent, prev, version) = match self.lines.lock().expect("lines").get(line) {
            Some(l) => (Some(l.tip), l.files.clone(), l.version),
            None => (None, BTreeMap::new(), 0),
        };
        let mut changes = Vec::new();
        let mut blobs = Vec::new();
        let mut paths: Vec<&String> = prev.keys().chain(files.keys()).collect();
        paths.sort();
        paths.dedup();
        for path in paths {
            let repo_path = RepoPath::new(path).expect("valid path");
            match (prev.get(path), files.get(path)) {
                (old, Some((kind, new))) if old != Some(&(*kind, new.clone())) => {
                    blobs.push(Frame::encode(ObjectKind::Blob, new));
                    changes.push(Change::Put {
                        path: repo_path,
                        kind: *kind,
                        id: BlobId::hash(new).0,
                    });
                }
                (Some(_), None) => changes.push(Change::Delete { path: repo_path }),
                _ => {}
            }
        }
        if changes.is_empty() {
            return parent.expect("a first commit has files");
        }
        let cx = admin_cx();
        self.api
            .put_objects(
                &cx,
                PutObjects {
                    repo: String::new(),
                    blobs,
                },
            )
            .await
            .expect("upload blobs");
        let base = parent.and_then(|p| self.root(p)).and_then(non_empty);
        let applied = apply_changes_with(
            &FakeSource(&self.api),
            base,
            &changes,
            &ApplyOptions { case_clash: true },
        )
        .await
        .expect("apply changes");
        let changeset = Changeset {
            parents: parent.into_iter().collect(),
            root: applied.root.unwrap_or_else(empty_root),
            changes,
            author: Author {
                name: "test".to_owned(),
                email: "test@example.com".to_owned(),
                kind: AuthorKind::Human,
            },
            authored_at: Timestamp {
                ms: version as i64,
                offset_min: 0,
            },
            message: format!("{line} {}", version + 1),
            metadata: Metadata::new(),
        };
        let id = changeset.id();
        self.api
            .import_changesets(
                &cx,
                ImportChangesets {
                    repo: String::new(),
                    changesets: vec![changeset],
                },
            )
            .await
            .expect("import changeset");
        self.lines.lock().expect("lines").insert(
            line.to_owned(),
            Line {
                tip: id,
                version: version + 1,
                files: files.clone(),
            },
        );
        id
    }

    fn root_of(&self, id: ChangesetId) -> ApiResult<TreeId> {
        self.root(id).ok_or_else(|| ApiError::NotFound {
            what: format!("changeset {id}"),
        })
    }
}

impl ObjectApi for SnapshotRepo {
    async fn get_objects(&self, cx: &CallContext, mut req: GetObjects) -> ApiResult<Objects> {
        let mut raw_found = Vec::new();
        {
            let raw = self.raw_objects.lock().expect("raw");
            req.ids.retain(|id| match raw.get(id) {
                Some(f) => {
                    raw_found.push((*id, f.clone()));
                    false
                }
                None => true,
            });
        }
        let mut out = if req.ids.is_empty() {
            Objects::default()
        } else {
            self.api.get_objects(cx, req).await?
        };
        out.found.extend(raw_found);
        for i in 0..self.padding.load(Ordering::SeqCst) {
            let bytes = format!("unrequested {i}").into_bytes();
            out.found.push((
                BlobId::hash(&bytes).0,
                Frame::encode(ObjectKind::Blob, &bytes),
            ));
        }
        Ok(out)
    }

    async fn has_objects(&self, cx: &CallContext, req: HasObjects) -> ApiResult<Vec<ObjectId>> {
        self.take_failure("HasObjects")?;
        self.api.has_objects(cx, req).await
    }

    async fn put_objects(&self, cx: &CallContext, req: PutObjects) -> ApiResult<Vec<ObjectId>> {
        self.take_failure("PutObjects")?;
        self.api.put_objects(cx, req).await
    }
}

impl RepoApi for SnapshotRepo {
    async fn create_changeset(
        &self,
        _cx: &CallContext,
        req: CreateChangeset,
    ) -> ApiResult<Created> {
        self.create(req).await
    }

    async fn get_changesets(
        &self,
        cx: &CallContext,
        mut req: GetChangesets,
    ) -> ApiResult<Changesets> {
        self.take_failure("GetChangesets")?;
        let mut raw_found = Vec::new();
        {
            let raw = self.raw_changesets.lock().expect("raw");
            req.ids.retain(|id| match raw.get(id) {
                Some(c) => {
                    raw_found.push(ChangesetView {
                        changeset: c.clone(),
                        lsn: 0,
                        state: DraftState::Landed,
                        superseded_by: None,
                    });
                    false
                }
                None => true,
            });
        }
        let mut out = if req.ids.is_empty() {
            Changesets::default()
        } else {
            self.api.get_changesets(cx, req).await?
        };
        self.restate(&mut out);
        out.found.extend(raw_found);
        Ok(out)
    }

    async fn resolve_ref(&self, _cx: &CallContext, req: ResolveRef) -> ApiResult<RefView> {
        let lines = self.lines.lock().expect("lines");
        let line = lines.get(&req.name).ok_or_else(|| ApiError::NotFound {
            what: format!("ref {}", req.name),
        })?;
        Ok(RefView {
            name: req.name,
            target: line.tip,
            version: line.version,
            kind: RefKind::Line,
        })
    }

    async fn list_refs(&self, _cx: &CallContext, req: ListRefs) -> ApiResult<Vec<RefView>> {
        let prefix = req.prefix.unwrap_or_default();
        let limit = req.page.limit.clamp(1, 1000) as usize;
        Ok(self
            .lines
            .lock()
            .expect("lines")
            .iter()
            .filter(|(name, _)| name.starts_with(&prefix))
            .take(limit)
            .map(|(name, l)| RefView {
                name: name.clone(),
                target: l.tip,
                version: l.version,
                kind: RefKind::Line,
            })
            .collect())
    }

    async fn update_ref(&self, _cx: &CallContext, _req: UpdateRef) -> ApiResult<RefView> {
        rejected()
    }

    /// Forks a line at another line's tip (the fake knows files only
    /// there); an existing name is a `RefConflict`, as in the engine.
    async fn create_line(&self, _cx: &CallContext, req: CreateLine) -> ApiResult<RefView> {
        let mut lines = self.lines.lock().expect("lines");
        if let Some(l) = lines.get(&req.name) {
            return Err(ApiError::RefConflict {
                name: req.name,
                actual: Some(l.tip),
            });
        }
        let files = lines
            .values()
            .find(|l| l.tip == req.from)
            .map(|l| l.files.clone())
            .ok_or_else(|| ApiError::NotFound {
                what: format!("changeset {}", req.from),
            })?;
        lines.insert(
            req.name.clone(),
            Line {
                tip: req.from,
                version: 1,
                files,
            },
        );
        Ok(RefView {
            name: req.name,
            target: req.from,
            version: 1,
            kind: RefKind::Line,
        })
    }

    async fn stat(&self, _cx: &CallContext, _req: PathAt) -> ApiResult<EntryView> {
        rejected()
    }

    async fn list_tree(&self, _cx: &CallContext, _req: ListTree) -> ApiResult<Entries> {
        rejected()
    }

    async fn read_file(&self, _cx: &CallContext, _req: PathAt) -> ApiResult<FileContent> {
        rejected()
    }

    async fn log(&self, _cx: &CallContext, _req: Log) -> ApiResult<History> {
        rejected()
    }

    async fn is_ancestor(&self, _cx: &CallContext, _req: Pair) -> ApiResult<bool> {
        rejected()
    }

    async fn merge_base(&self, _cx: &CallContext, _req: Pair) -> ApiResult<Option<ChangesetId>> {
        rejected()
    }

    /// The id-pruned tree diff, restricted to `paths`, paged by offset
    /// (fine for a fake; the engine pages by path).
    async fn diff(&self, _cx: &CallContext, req: Diff) -> ApiResult<DiffResult> {
        if let Some(lie) = self.diff_lie.lock().expect("diff").clone() {
            return Ok(lie);
        }
        let from = non_empty(self.root_of(req.from)?);
        let to = non_empty(self.root_of(req.to)?);
        let source = FakeSource(&self.api);
        let mut all = Vec::new();
        let mut after: Option<RepoPath> = None;
        loop {
            let page = diff_trees_after(&source, from, to, after.as_ref(), 1000)
                .await
                .map_err(|e| ApiError::Rejected {
                    policy: "fake".to_owned(),
                    detail: e.to_string(),
                })?;
            all.extend(page.changes);
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        all.retain(|c| req.paths.is_empty() || req.paths.iter().any(|p| c.path().starts_with(p)));
        let offset: usize = req
            .page
            .token
            .as_deref()
            .and_then(|t| t.parse().ok())
            .unwrap_or(0)
            .min(all.len());
        let limit = req.page.limit.clamp(1, 1000) as usize;
        let end = (offset + limit).min(all.len());
        Ok(DiffResult {
            files: all[offset..end]
                .iter()
                .map(|c| FileDiff {
                    change: c.clone(),
                    patch: None,
                    truncated: false,
                })
                .collect(),
            next: (end < all.len()).then(|| end.to_string()),
        })
    }

    async fn blame(&self, _cx: &CallContext, _req: Blame) -> ApiResult<BlameResult> {
        rejected()
    }

    async fn rebase(&self, _cx: &CallContext, _req: Rebase) -> ApiResult<ChangesetId> {
        rejected()
    }

    async fn revert(&self, _cx: &CallContext, _req: Revert) -> ApiResult<ChangesetId> {
        rejected()
    }

    async fn land(&self, _cx: &CallContext, req: Land) -> ApiResult<Landed> {
        self.do_land(req).await
    }

    async fn attest(&self, _cx: &CallContext, _req: Attest) -> ApiResult<()> {
        rejected()
    }

    async fn watch(&self, _cx: &CallContext, _req: Watch) -> ApiResult<JournalPage> {
        rejected()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_api::block_on;

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
        pairs
            .iter()
            .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn snapshot_repo_commits_and_diffs() {
        let repo = SnapshotRepo::new();
        let a = block_on(repo.commit("main", &files(&[("x/a", "1"), ("y/b", "2")])));
        let b = block_on(repo.commit("main", &files(&[("x/a", "1"), ("y/b", "3"), ("z", "4")])));
        assert_eq!(repo.tip("main"), Some(b));
        let cx = admin_cx();
        let diff = |paths: Vec<RepoPath>| {
            block_on(repo.diff(
                &cx,
                Diff {
                    repo: String::new(),
                    from: a,
                    to: b,
                    paths,
                    patch: None,
                    page: Page::default(),
                },
            ))
            .unwrap()
        };
        // Page limit 0 is clamped to 1: two pages.
        let first = diff(vec![]);
        assert_eq!(first.files.len(), 1);
        assert_eq!(first.next.as_deref(), Some("1"));
        let only_y = diff(vec![RepoPath::new("y").unwrap()]);
        assert_eq!(only_y.files[0].change.path().as_str(), "y/b");
        assert_eq!(only_y.next, None);
        let r = block_on(repo.resolve_ref(
            &cx,
            ResolveRef {
                repo: String::new(),
                name: "main".into(),
            },
        ))
        .unwrap();
        assert_eq!((r.target, r.version), (b, 2));
    }

    #[tokio::test]
    async fn refuses_a_client_without_a_protocol_version() {
        let server = TestServer::tcp(Arc::new(SnapshotRepo::new())).await;
        let mut client =
            pb::object_service_client::ObjectServiceClient::connect(server.url().to_owned())
                .await
                .unwrap();
        let status = client
            .has_objects(pb::HasObjectsRequest::default())
            .await
            .unwrap_err();
        assert_eq!(
            ApiError::from_status(&status),
            Some(ApiError::IncompatibleProtocol {
                server: PROTOCOL_VERSION.to_string()
            })
        );
        assert!(server.calls().is_empty());
    }
}
