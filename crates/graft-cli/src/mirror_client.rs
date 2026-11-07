//! A client of the server's mirror service (D41 §3.2, §3.5), which
//! `graft_client::Remote` does not carry: the settings, credential and
//! batch request calls behind `graft repo mirror`, `graft repo batch` and
//! the MCP mirror tools. Each request carries the protocol version, the
//! request id, the bearer token (marked sensitive) and a deadline, as the
//! remote's do.

use std::time::Duration;

use graft_client::Endpoint;
use graft_client::remote::{CONNECT_TIMEOUT, REQUEST_ID_HEADER};
use graft_mcp::{MirrorAdmin, MirrorFuture};
use graft_proto::pb;
use graft_proto::pb::mirror_service_client::MirrorServiceClient;
use graft_proto::{ApiError, CallContext, PROTOCOL_VERSION, ProtocolVersion, VERSION_HEADER};
use tonic::metadata::{MetadataMap, MetadataValue};
use tonic::transport::Channel;

use crate::error::Failure;

/// How long one mirror call may take.
const TIMEOUT: Duration = Duration::from_secs(30);

/// One connection to the mirror service.
pub(crate) struct MirrorClient {
    channel: Channel,
    token: Option<String>,
}

impl MirrorClient {
    /// Connects to `endpoint`. The mirror service is served by a graft
    /// server, never by a node socket or a local repository.
    pub(crate) async fn connect(
        endpoint: &Endpoint,
        token: Option<String>,
    ) -> Result<Self, Failure> {
        let Endpoint::Server(url) = endpoint else {
            return Err(Failure::usage(
                "mirror and batch commands need a graft server endpoint",
            ));
        };
        let channel = tonic::transport::Endpoint::from_shared(url.clone())
            .map_err(|_| Failure::usage("not a valid server URL"))?
            .connect_timeout(CONNECT_TIMEOUT)
            .tcp_nodelay(true)
            .connect()
            .await
            .map_err(|e| {
                Failure::new(
                    crate::Exit::Failure,
                    "connect",
                    format!("could not connect to {url}: {e}"),
                )
            })?;
        Ok(Self { channel, token })
    }

    fn client(&self) -> MirrorServiceClient<Channel> {
        MirrorServiceClient::new(self.channel.clone())
    }

    fn request<T>(&self, cx: &CallContext, msg: T) -> Result<tonic::Request<T>, ApiError> {
        let mut req = tonic::Request::new(msg);
        let md = req.metadata_mut();
        md.insert(
            VERSION_HEADER,
            MetadataValue::try_from(PROTOCOL_VERSION.to_string()).expect("version is ASCII"),
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
        req.set_timeout(TIMEOUT);
        Ok(req)
    }
}

/// Refuses an answer from a server whose version this client can't bind to.
fn check_version(md: &MetadataMap) -> Result<(), ApiError> {
    let header = md.get(VERSION_HEADER).and_then(|v| v.to_str().ok());
    match header.map(str::parse::<ProtocolVersion>) {
        Some(Ok(server)) if PROTOCOL_VERSION.binds_to(server) => Ok(()),
        _ => Err(ApiError::IncompatibleProtocol {
            server: header.unwrap_or("none").chars().take(16).collect(),
        }),
    }
}

/// The typed error a status carries, else the closest by code.
fn status_error(s: tonic::Status) -> ApiError {
    if let Some(e) = ApiError::from_status(&s) {
        return e;
    }
    match s.code() {
        tonic::Code::Unauthenticated => ApiError::Unauthenticated,
        tonic::Code::PermissionDenied => ApiError::PermissionDenied {
            scope: String::new(),
            path: None,
        },
        tonic::Code::Unimplemented => ApiError::IncompatibleProtocol {
            server: "no mirror service".into(),
        },
        _ => ApiError::Unavailable {
            retry_after_ms: 1_000,
        },
    }
}

/// One unary call: build the request, send it, check the version.
macro_rules! unary {
    ($self:ident, $cx:ident, $method:ident, $msg:expr) => {
        Box::pin(async move {
            let req = $self.request($cx, $msg)?;
            let resp = $self.client().$method(req).await.map_err(status_error)?;
            check_version(resp.metadata())?;
            Ok(resp.into_inner())
        })
    };
}

impl MirrorAdmin for MirrorClient {
    fn set_mirror<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::SetMirrorRequest,
    ) -> MirrorFuture<'a, pb::Mirror> {
        unary!(self, cx, set_mirror, req)
    }
    fn get_mirror<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::MirrorKey,
    ) -> MirrorFuture<'a, pb::Mirror> {
        unary!(self, cx, get_mirror, req)
    }
    fn list_mirrors<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::ListMirrorsRequest,
    ) -> MirrorFuture<'a, pb::MirrorPage> {
        unary!(self, cx, list_mirrors, req)
    }
    fn remove_mirror<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::RemoveMirrorRequest,
    ) -> MirrorFuture<'a, pb::RemoveMirrorResult> {
        unary!(self, cx, remove_mirror, req)
    }
    fn purge_mirror<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::PurgeMirrorRequest,
    ) -> MirrorFuture<'a, pb::MirrorRequest> {
        unary!(self, cx, purge_mirror, req)
    }
    fn set_mirror_credential<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::SetMirrorCredentialRequest,
    ) -> MirrorFuture<'a, pb::Mirror> {
        unary!(self, cx, set_mirror_credential, req)
    }
    fn cut_batch<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::CutBatchRequest,
    ) -> MirrorFuture<'a, pb::MirrorRequest> {
        unary!(self, cx, cut_batch, req)
    }
    fn get_batch<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::BatchKey,
    ) -> MirrorFuture<'a, pb::BatchView> {
        unary!(self, cx, get_batch, req)
    }
    fn list_batches<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::ListBatchesRequest,
    ) -> MirrorFuture<'a, pb::BatchPage> {
        unary!(self, cx, list_batches, req)
    }
    fn cancel_request<'a>(
        &'a self,
        cx: &'a CallContext,
        req: pb::CancelRequestRequest,
    ) -> MirrorFuture<'a, pb::MirrorRequest> {
        unary!(self, cx, cancel_request, req)
    }
}
