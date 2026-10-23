//! The remote API client over gRPC (decision D4).
//!
//! One [`Remote`] holds one HTTP/2 channel, over TCP or the node's Unix
//! socket; the channel reconnects by itself after a dropped connection.
//! Every request carries the protocol version ([`VERSION_HEADER`]), the
//! request id, the bearer token when one is set, and a deadline (the call
//! context's, else [`DEFAULT_TIMEOUT`]). Repository and workspace messages
//! travel gzip-compressed; object messages do not, since frames are already
//! compressed (`docs/design.md` §6.3), but the client accepts either.

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use graft_model::{ChangesetId, ObjectId};
use graft_proto::api::{ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::error::ApiResult;
use graft_proto::frame::{HEADER_LEN, MAX_FRAME_PAYLOAD};
use graft_proto::pb;
use graft_proto::pb::object_service_client::ObjectServiceClient;
use graft_proto::pb::repo_service_client::RepoServiceClient;
use graft_proto::pb::workspace_service_client::WorkspaceServiceClient;
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

/// The most frame bytes one `GetObjects` answer may carry: the server's
/// 64 MiB response bound (`docs/design.md` §7.2) plus one largest frame,
/// which may straddle it.
pub const MAX_GET_OBJECTS_BYTES: u64 = 64 * 1024 * 1024 + MAX_FRAME_PAYLOAD + HEADER_LEN as u64;

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
    max_response_bytes: u64,
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
                // Explicit, not left to tonic's default: with Nagle on, a
                // small request can wait out the server's delayed ACK.
                .tcp_nodelay(true)
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
            max_response_bytes: MAX_GET_OBJECTS_BYTES,
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

    /// Caps the frame bytes of one `GetObjects` answer below
    /// [`MAX_GET_OBJECTS_BYTES`] (never above it).
    pub fn with_max_response_bytes(mut self, bytes: u64) -> Self {
        self.max_response_bytes = bytes.min(MAX_GET_OBJECTS_BYTES);
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

    fn workspaces(&self) -> WorkspaceServiceClient<Channel> {
        WorkspaceServiceClient::new(self.channel.clone())
            .send_compressed(CompressionEncoding::Gzip)
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

    /// Refuses a response that advertises no version (a peer that is not
    /// graft, or one that strips the header) or a version this client can't
    /// bind to.
    fn check_version(&self, md: &MetadataMap) -> ApiResult<()> {
        let Some(v) = md.get(VERSION_HEADER) else {
            return Err(ApiError::IncompatibleProtocol {
                server: "none".to_owned(),
            });
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

fn raw_id(b: &[u8]) -> Option<ObjectId> {
    <[u8; 32]>::try_from(b).ok().map(ObjectId)
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

/// The status of a refused `PutObjects`. tonic refuses a message over the
/// server's decode limit as `OutOfRange` with no graft error attached; that
/// is the typed `LimitExceeded`, naming the limit tonic's text gives (or, if
/// the text has none, the client's own message ceiling).
fn put_error(s: tonic::Status) -> ApiError {
    if s.code() == tonic::Code::OutOfRange && ApiError::from_status(&s).is_none() {
        return ApiError::LimitExceeded {
            limit: "put-objects-message".to_owned(),
            max: decode_limit(s.message()).unwrap_or(MAX_MESSAGE_BYTES as u64),
        };
    }
    status_error(s)
}

/// The byte limit in tonic's decode refusal, "… the limit is: N bytes".
fn decode_limit(message: &str) -> Option<u64> {
    let tail = message.split("the limit is: ").nth(1)?;
    tail.split_whitespace().next()?.parse().ok()
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
    /// Collects every page of the stream, keeping only ids that were asked
    /// for, each once, as the pages arrive. The answer is bounded: at most
    /// one page per requested id plus one, and at most
    /// [`MAX_GET_OBJECTS_BYTES`] of frames (or less, see
    /// [`Remote::with_max_response_bytes`]); past either the call fails with
    /// `LimitExceeded`. A dropped stream is an error; the caller asks again
    /// for what it still lacks.
    async fn get_objects(&self, cx: &CallContext, req: GetObjects) -> ApiResult<Objects> {
        let mut asked: HashSet<ObjectId> = req.ids.iter().copied().collect();
        let max_pages = req.ids.len() as u64 + 1;
        let req = self.request(cx, pb::GetObjectsRequest::from(req))?;
        let resp = self
            .objects()
            .get_objects(req)
            .await
            .map_err(status_error)?;
        self.check_version(resp.metadata())?;
        let mut stream = resp.into_inner();
        let mut out = Objects::default();
        let (mut pages, mut bytes) = (0u64, 0u64);
        while let Some(mut page) = stream.message().await.map_err(status_error)? {
            pages += 1;
            if pages > max_pages {
                return Err(ApiError::LimitExceeded {
                    limit: "get-objects-pages".to_owned(),
                    max: max_pages,
                });
            }
            bytes += page.found.iter().map(|f| f.frame.len() as u64).sum::<u64>();
            if bytes > self.max_response_bytes {
                return Err(ApiError::LimitExceeded {
                    limit: "get-objects-bytes".to_owned(),
                    max: self.max_response_bytes,
                });
            }
            // Unrequested or repeated ids are dropped before they are parsed.
            page.found
                .retain(|f| raw_id(&f.id).is_some_and(|id| asked.remove(&id)));
            let requested = |b: &Vec<u8>| raw_id(b).is_some_and(|id| asked.contains(&id));
            page.missing.retain(requested);
            page.deferred.retain(requested);
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
        let req = self.request(cx, tokio_stream::iter(msgs))?;
        let resp = self.objects().put_objects(req).await.map_err(put_error)?;
        self.check_version(resp.metadata())?;
        Vec::<ObjectId>::try_from(resp.into_inner()).map_err(wire_error)
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

    async fn create_line(&self, cx: &CallContext, req: CreateLine) -> ApiResult<RefView> {
        let r = unary!(
            self,
            cx,
            repo,
            create_line,
            pb::CreateLineRequest::from(req)
        );
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
        cx: &CallContext,
        req: OpenWorkspace,
    ) -> ApiResult<WorkspaceView> {
        let r = unary!(
            self,
            cx,
            workspaces,
            open_workspace,
            pb::OpenWorkspaceRequest::from(req)
        );
        WorkspaceView::try_from(r).map_err(wire_error)
    }

    async fn get_workspace(&self, cx: &CallContext, req: WorkspaceRef) -> ApiResult<WorkspaceView> {
        let r = unary!(
            self,
            cx,
            workspaces,
            get_workspace,
            pb::WorkspaceRef::from(req)
        );
        WorkspaceView::try_from(r).map_err(wire_error)
    }

    async fn list_workspaces(
        &self,
        cx: &CallContext,
        repo: String,
        page: Page,
    ) -> ApiResult<Vec<WorkspaceView>> {
        let msg = pb::ListWorkspacesRequest {
            repo,
            page: Some(page.into()),
        };
        let r = unary!(self, cx, workspaces, list_workspaces, msg);
        Vec::<WorkspaceView>::try_from(r).map_err(wire_error)
    }

    async fn renew_lease(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        lease_ms: u64,
    ) -> ApiResult<WorkspaceView> {
        let msg = pb::RenewLeaseRequest {
            workspace: Some(req.into()),
            lease_ms,
        };
        let r = unary!(self, cx, workspaces, renew_lease, msg);
        WorkspaceView::try_from(r).map_err(wire_error)
    }

    async fn set_head(&self, cx: &CallContext, req: SetHead) -> ApiResult<WorkspaceView> {
        let r = unary!(
            self,
            cx,
            workspaces,
            set_head,
            pb::SetHeadRequest::from(req)
        );
        WorkspaceView::try_from(r).map_err(wire_error)
    }

    async fn sync(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        onto: Option<ChangesetId>,
    ) -> ApiResult<Synced> {
        let msg = pb::SyncRequest {
            workspace: Some(req.into()),
            onto: onto.map(|id| id.0.0.to_vec()),
        };
        let r = unary!(self, cx, workspaces, sync, msg);
        Synced::try_from(r).map_err(wire_error)
    }

    async fn stack(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        op: StackOp,
    ) -> ApiResult<WorkspaceView> {
        let msg = pb::StackRequest {
            workspace: Some(req.into()),
            op: Some(op.into()),
        };
        let r = unary!(self, cx, workspaces, stack, msg);
        WorkspaceView::try_from(r).map_err(wire_error)
    }

    async fn close_workspace(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        keep_drafts: bool,
    ) -> ApiResult<()> {
        let msg = pb::CloseWorkspaceRequest {
            workspace: Some(req.into()),
            keep_drafts,
        };
        unary!(self, cx, workspaces, close_workspace, msg);
        Ok(())
    }

    async fn get_conflicts(&self, cx: &CallContext, req: GetConflicts) -> ApiResult<Conflicts> {
        let r = unary!(
            self,
            cx,
            workspaces,
            get_conflicts,
            pb::GetConflictsRequest::from(req)
        );
        Conflicts::try_from(r).map_err(wire_error)
    }

    async fn resolve(&self, cx: &CallContext, req: Resolve) -> ApiResult<Resolved> {
        let r = unary!(self, cx, workspaces, resolve, pb::ResolveRequest::from(req));
        Resolved::try_from(r).map_err(wire_error)
    }
}
