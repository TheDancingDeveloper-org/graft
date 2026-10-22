//! The remote API client over gRPC (decision D4).
//!
//! One [`Remote`] holds one HTTP/2 channel, over TCP or the node's Unix
//! socket; the channel reconnects by itself after a dropped connection.
//! Every request carries the protocol version ([`VERSION_HEADER`]), the
//! request id, the bearer token when one is set, and a deadline (the call
//! context's, else [`DEFAULT_TIMEOUT`]). Repository and workspace messages
//! travel gzip-compressed; object messages do not, since frames are already
//! compressed (`docs/design.md` §6.3), but the client accepts either.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use graft_model::{ChangesetId, ObjectId};
use graft_proto::api::{ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::error::ApiResult;
use graft_proto::pb;
use graft_proto::pb::object_service_client::ObjectServiceClient;
use graft_proto::pb::repo_service_client::RepoServiceClient;
use graft_proto::types::*;
use graft_proto::{
    ApiError, CallContext, PROTOCOL_VERSION, ProtocolVersion, VERSION_HEADER, WireError,
};
use hyper_util::rt::TokioIo;
use tonic::codec::CompressionEncoding;
use tonic::metadata::{MetadataMap, MetadataValue};
use tonic::transport::{Channel, Uri};

use crate::{Endpoint, Error};

/// The per-call deadline when the call context sets none.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait for a connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The largest message either side accepts: a 64 MiB frame plus headroom.
pub const MAX_MESSAGE_BYTES: usize = 72 * 1024 * 1024;

/// The most frame bytes sent in one `PutObjects` stream message.
pub const PUT_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// The request header carrying the client's request id.
pub const REQUEST_ID_HEADER: &str = "graft-request-id";

/// The wait suggested when the transport fails without a hint.
const TRANSPORT_RETRY_MS: u64 = 1000;

/// A connection to a server or node.
pub struct Remote {
    endpoint: Endpoint,
    channel: Channel,
    token: Option<String>,
    protocol: ProtocolVersion,
    timeout: Duration,
}

impl fmt::Debug for Remote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Remote")
            .field("endpoint", &self.endpoint)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("protocol", &self.protocol)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl Remote {
    /// Connects to `endpoint`. The protocol version is checked on every
    /// call: the server refuses a client it can't bind with
    /// `IncompatibleProtocol`, and the client refuses a server whose
    /// advertised version it can't bind to.
    pub async fn connect(endpoint: Endpoint) -> Result<Self, Error> {
        let channel = match &endpoint {
            Endpoint::Server(url) => tonic::transport::Endpoint::from_shared(url.clone())
                .map_err(|_| Error::Endpoint("not a valid server URL".to_owned()))?
                .connect_timeout(CONNECT_TIMEOUT)
                .connect()
                .await
                .map_err(|_| Error::Api(unavailable()))?,
            Endpoint::NodeSocket(path) => {
                let path: PathBuf = path.clone();
                // The URI is required by the API and unused by the connector.
                tonic::transport::Endpoint::from_static("http://node.sock")
                    .connect_timeout(CONNECT_TIMEOUT)
                    .connect_with_connector(tower::service_fn(move |_: Uri| {
                        let path = path.clone();
                        async move {
                            let stream = tokio::net::UnixStream::connect(path).await?;
                            Ok::<_, io::Error>(TokioIo::new(stream))
                        }
                    }))
                    .await
                    .map_err(|_| Error::Api(unavailable()))?
            }
        };
        Ok(Self {
            endpoint,
            channel,
            token: None,
            protocol: PROTOCOL_VERSION,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// Sends `token` as the bearer credential on every call.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Sets the deadline used when the call context has none.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Speaks `version` instead of this build's [`PROTOCOL_VERSION`] (for
    /// compatibility tests).
    pub fn with_protocol_version(mut self, version: ProtocolVersion) -> Self {
        self.protocol = version;
        self
    }

    /// The endpoint in use.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn objects(&self) -> ObjectServiceClient<Channel> {
        ObjectServiceClient::new(self.channel.clone())
            .accept_compressed(CompressionEncoding::Gzip)
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES)
    }

    fn repo(&self) -> RepoServiceClient<Channel> {
        RepoServiceClient::new(self.channel.clone())
            .send_compressed(CompressionEncoding::Gzip)
            .accept_compressed(CompressionEncoding::Gzip)
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES)
    }

    /// Wraps `msg` with the version, request id, token and deadline.
    fn request<T>(&self, cx: &CallContext, msg: T) -> ApiResult<tonic::Request<T>> {
        let mut req = tonic::Request::new(msg);
        let md = req.metadata_mut();
        md.insert(
            VERSION_HEADER,
            MetadataValue::try_from(self.protocol.to_string()).expect("version is ASCII"),
        );
        if let Ok(v) = MetadataValue::try_from(cx.request_id.as_str()) {
            md.insert(REQUEST_ID_HEADER, v);
        }
        if let Some(token) = &self.token {
            let mut v = MetadataValue::try_from(format!("Bearer {token}"))
                .map_err(|_| ApiError::Unauthenticated)?;
            v.set_sensitive(true);
            md.insert("authorization", v);
        }
        let timeout = match cx.deadline_ms {
            Some(deadline) => {
                let left = deadline.saturating_sub(now_ms());
                if left <= 0 {
                    return Err(ApiError::Unavailable { retry_after_ms: 0 });
                }
                Duration::from_millis(left as u64)
            }
            None => self.timeout,
        };
        req.set_timeout(timeout);
        Ok(req)
    }

    /// Refuses a server that advertises a version this client can't bind to.
    fn check_version(&self, md: &MetadataMap) -> ApiResult<()> {
        let Some(v) = md.get(VERSION_HEADER) else {
            return Ok(());
        };
        let text = v.to_str().unwrap_or("?");
        match text.parse::<ProtocolVersion>() {
            Ok(server) if self.protocol.binds_to(server) => Ok(()),
            _ => Err(ApiError::IncompatibleProtocol {
                server: text.chars().take(16).collect(),
            }),
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn unavailable() -> ApiError {
    ApiError::Unavailable {
        retry_after_ms: TRANSPORT_RETRY_MS,
    }
}

/// Maps a status to the typed error it carries, or, for a status with no
/// graft error (a transport failure, a deadline, a non-graft peer), to the
/// closest typed error by code.
fn status_error(s: tonic::Status) -> ApiError {
    if let Some(e) = ApiError::from_status(&s) {
        return e;
    }
    use tonic::Code;
    match s.code() {
        // A boundary conversion failure (`WireError`) names the field only.
        Code::InvalidArgument => ApiError::InvalidChange {
            path: String::new(),
            rule: s.message().chars().take(128).collect(),
        },
        Code::Unauthenticated => ApiError::Unauthenticated,
        Code::PermissionDenied => ApiError::PermissionDenied {
            scope: String::new(),
            path: None,
        },
        _ => unavailable(),
    }
}

/// A response that does not convert: the peer broke the protocol.
fn wire_error(e: WireError) -> ApiError {
    ApiError::Rejected {
        policy: "protocol".to_owned(),
        detail: format!("malformed response: {e}"),
    }
}

/// One unary call: wrap, send, check the version, unwrap.
macro_rules! unary {
    ($self:ident, $cx:ident, $client:ident, $method:ident, $msg:expr) => {{
        let req = $self.request($cx, $msg)?;
        let resp = $self.$client().$method(req).await.map_err(status_error)?;
        $self.check_version(resp.metadata())?;
        resp.into_inner()
    }};
}

impl ObjectApi for Remote {
    /// Collects every page of the stream. A dropped stream is an error; the
    /// caller asks again for what it still lacks.
    async fn get_objects(&self, cx: &CallContext, req: GetObjects) -> ApiResult<Objects> {
        let req = self.request(cx, pb::GetObjectsRequest::from(req))?;
        let resp = self
            .objects()
            .get_objects(req)
            .await
            .map_err(status_error)?;
        self.check_version(resp.metadata())?;
        let mut stream = resp.into_inner();
        let mut out = Objects::default();
        while let Some(page) = stream.message().await.map_err(status_error)? {
            let page = Objects::try_from(page).map_err(wire_error)?;
            out.found.extend(page.found);
            out.missing.extend(page.missing);
            out.deferred.extend(page.deferred);
        }
        Ok(out)
    }

    async fn has_objects(&self, cx: &CallContext, req: HasObjects) -> ApiResult<Vec<ObjectId>> {
        let ids = unary!(
            self,
            cx,
            objects,
            has_objects,
            pb::HasObjectsRequest::from(req)
        );
        Vec::<ObjectId>::try_from(ids).map_err(wire_error)
    }

    /// Streams the blobs in messages of at most [`PUT_CHUNK_BYTES`] of frames
    /// (a larger single frame goes alone).
    async fn put_objects(&self, cx: &CallContext, req: PutObjects) -> ApiResult<Vec<ObjectId>> {
        let mut msgs = Vec::new();
        let mut cur = pb::PutObjectsRequest {
            repo: req.repo.clone(),
            blobs: Vec::new(),
        };
        let mut size = 0;
        for frame in &req.blobs {
            let bytes = frame.to_bytes();
            if !cur.blobs.is_empty() && size + bytes.len() > PUT_CHUNK_BYTES {
                msgs.push(std::mem::replace(
                    &mut cur,
                    pb::PutObjectsRequest {
                        repo: req.repo.clone(),
                        blobs: Vec::new(),
                    },
                ));
                size = 0;
            }
            size += bytes.len();
            cur.blobs.push(bytes);
        }
        msgs.push(cur);
        let ids = unary!(self, cx, objects, put_objects, tokio_stream::iter(msgs));
        Vec::<ObjectId>::try_from(ids).map_err(wire_error)
    }
}

impl RepoApi for Remote {
    async fn create_changeset(&self, cx: &CallContext, req: CreateChangeset) -> ApiResult<Created> {
        let r = unary!(
            self,
            cx,
            repo,
            create_changeset,
            pb::CreateChangesetRequest::from(req)
        );
        Created::try_from(r).map_err(wire_error)
    }

    async fn get_changesets(&self, cx: &CallContext, req: GetChangesets) -> ApiResult<Changesets> {
        let r = unary!(
            self,
            cx,
            repo,
            get_changesets,
            pb::GetChangesetsRequest::from(req)
        );
        Changesets::try_from(r).map_err(wire_error)
    }

    async fn resolve_ref(&self, cx: &CallContext, req: ResolveRef) -> ApiResult<RefView> {
        let r = unary!(
            self,
            cx,
            repo,
            resolve_ref,
            pb::ResolveRefRequest::from(req)
        );
        RefView::try_from(r).map_err(wire_error)
    }

    async fn list_refs(&self, cx: &CallContext, req: ListRefs) -> ApiResult<Vec<RefView>> {
        let r = unary!(self, cx, repo, list_refs, pb::ListRefsRequest::from(req));
        Vec::<RefView>::try_from(r).map_err(wire_error)
    }

    async fn update_ref(&self, cx: &CallContext, req: UpdateRef) -> ApiResult<RefView> {
        let r = unary!(self, cx, repo, update_ref, pb::UpdateRefRequest::from(req));
        RefView::try_from(r).map_err(wire_error)
    }

    async fn stat(&self, cx: &CallContext, req: PathAt) -> ApiResult<EntryView> {
        let r = unary!(self, cx, repo, stat, pb::PathAt::from(req));
        EntryView::try_from(r).map_err(wire_error)
    }

    async fn list_tree(&self, cx: &CallContext, req: ListTree) -> ApiResult<Entries> {
        let r = unary!(self, cx, repo, list_tree, pb::ListTreeRequest::from(req));
        Entries::try_from(r).map_err(wire_error)
    }

    async fn read_file(&self, cx: &CallContext, req: PathAt) -> ApiResult<FileContent> {
        let r = unary!(self, cx, repo, read_file, pb::PathAt::from(req));
        FileContent::try_from(r).map_err(wire_error)
    }

    async fn log(&self, cx: &CallContext, req: Log) -> ApiResult<History> {
        let r = unary!(self, cx, repo, log, pb::LogRequest::from(req));
        History::try_from(r).map_err(wire_error)
    }

    async fn is_ancestor(&self, cx: &CallContext, req: Pair) -> ApiResult<bool> {
        Ok(unary!(self, cx, repo, is_ancestor, pb::Pair::from(req)).value)
    }

    async fn merge_base(&self, cx: &CallContext, req: Pair) -> ApiResult<Option<ChangesetId>> {
        let r = unary!(self, cx, repo, merge_base, pb::Pair::from(req));
        match r.id {
            None => Ok(None),
            Some(b) => <[u8; 32]>::try_from(b.as_slice())
                .map(|b| Some(ChangesetId(ObjectId(b))))
                .map_err(|_| {
                    wire_error(WireError {
                        field: "id",
                        reason: "id is not 32 bytes",
                    })
                }),
        }
    }

    async fn diff(&self, cx: &CallContext, req: Diff) -> ApiResult<DiffResult> {
        let r = unary!(self, cx, repo, diff, pb::DiffRequest::from(req));
        DiffResult::try_from(r).map_err(wire_error)
    }

    async fn blame(&self, cx: &CallContext, req: Blame) -> ApiResult<BlameResult> {
        let r = unary!(self, cx, repo, blame, pb::BlameRequest::from(req));
        BlameResult::try_from(r).map_err(wire_error)
    }

    async fn rebase(&self, cx: &CallContext, req: Rebase) -> ApiResult<ChangesetId> {
        let r = unary!(self, cx, repo, rebase, pb::RebaseRequest::from(req));
        changeset_ref(r)
    }

    async fn revert(&self, cx: &CallContext, req: Revert) -> ApiResult<ChangesetId> {
        let r = unary!(self, cx, repo, revert, pb::RevertRequest::from(req));
        changeset_ref(r)
    }

    async fn land(&self, cx: &CallContext, req: Land) -> ApiResult<Landed> {
        let r = unary!(self, cx, repo, land, pb::LandRequest::from(req));
        Landed::try_from(r).map_err(wire_error)
    }

    async fn attest(&self, cx: &CallContext, req: Attest) -> ApiResult<()> {
        unary!(self, cx, repo, attest, pb::AttestRequest::from(req));
        Ok(())
    }

    /// Returns the first page of the journal stream.
    async fn watch(&self, cx: &CallContext, req: Watch) -> ApiResult<JournalPage> {
        let from_lsn = req.from_lsn;
        let mut stream = unary!(self, cx, repo, watch, pb::WatchRequest::from(req));
        Ok(match stream.message().await.map_err(status_error)? {
            Some(page) => JournalPage::from(page),
            None => JournalPage {
                entries: Vec::new(),
                next_lsn: from_lsn,
            },
        })
    }
}

fn changeset_ref(r: pb::ChangesetRef) -> ApiResult<ChangesetId> {
    <[u8; 32]>::try_from(r.id.as_slice())
        .map(|b| ChangesetId(ObjectId(b)))
        .map_err(|_| {
            wire_error(WireError {
                field: "id",
                reason: "id is not 32 bytes",
            })
        })
}

impl WorkspaceApi for Remote {
    async fn open_workspace(
        &self,
        _cx: &CallContext,
        _req: OpenWorkspace,
    ) -> ApiResult<WorkspaceView> {
        todo!("G2.5")
    }
    async fn get_workspace(
        &self,
        _cx: &CallContext,
        _req: WorkspaceRef,
    ) -> ApiResult<WorkspaceView> {
        todo!("G2.5")
    }
    async fn list_workspaces(
        &self,
        _cx: &CallContext,
        _repo: String,
        _page: Page,
    ) -> ApiResult<Vec<WorkspaceView>> {
        todo!("G2.5")
    }
    async fn renew_lease(
        &self,
        _cx: &CallContext,
        _req: WorkspaceRef,
        _lease_ms: u64,
    ) -> ApiResult<WorkspaceView> {
        todo!("G2.5")
    }
    async fn set_head(&self, _cx: &CallContext, _req: SetHead) -> ApiResult<WorkspaceView> {
        todo!("G2.5")
    }
    async fn sync(
        &self,
        _cx: &CallContext,
        _req: WorkspaceRef,
        _onto: Option<ChangesetId>,
    ) -> ApiResult<Synced> {
        todo!("G2.5")
    }
    async fn stack(
        &self,
        _cx: &CallContext,
        _req: WorkspaceRef,
        _op: StackOp,
    ) -> ApiResult<WorkspaceView> {
        todo!("G3.5")
    }
    async fn close_workspace(
        &self,
        _cx: &CallContext,
        _req: WorkspaceRef,
        _keep_drafts: bool,
    ) -> ApiResult<()> {
        todo!("G2.5")
    }
}
