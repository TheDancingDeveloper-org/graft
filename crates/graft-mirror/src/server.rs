//! The graft server, as the mirror sees it: `graft-client`'s [`Remote`] for
//! the object, repository and workspace services, plus the admin service,
//! which the import needs (`ImportChangesets`, `CreateRepo`) and `Remote`
//! does not speak yet. Every admin request carries the protocol version, the
//! request id, the bearer token (marked sensitive) and a deadline, as
//! `Remote`'s do.
//! Once `Remote` implements `AdminApi`, this module reduces to that.
//! It also speaks the server-only `MirrorService` ([`ExportLog`]).

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use graft_client::remote::{DEFAULT_TIMEOUT, MAX_MESSAGE_BYTES, REQUEST_ID_HEADER};
use graft_client::{Endpoint, LocalRepo, Remote};
use graft_git::GitSha;
use graft_model::{ChangesetId, ObjectId};
use graft_proto::api::{AdminApi, ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::error::ApiResult;
use graft_proto::pb;
use graft_proto::pb::admin_service_client::AdminServiceClient;
use graft_proto::pb::mirror_service_client::MirrorServiceClient;
use graft_proto::types::*;
use graft_proto::{ApiError, CallContext, PROTOCOL_VERSION, VERSION_HEADER};
use std::path::Path;
use std::sync::Arc;
use tonic::codec::CompressionEncoding;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

use crate::{ExportLog, MirrorError};

/// The wait suggested when the transport fails without a hint.
const TRANSPORT_RETRY_MS: u64 = 1000;

/// How long to wait for the admin connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A connection to a graft server, with the admin service.
pub struct Server {
    remote: Remote,
    admin: Channel,
    token: Option<String>,
}

/// A graft API: a remote server, or a local repository opened in-process.
pub enum Api {
    /// gRPC to a graft server.
    Remote(Server),
    /// The engine of a `local:` repository. The lock lives as long as this does.
    Local(Arc<LocalRepo>),
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("remote", &self.remote)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

fn unavailable() -> ApiError {
    ApiError::Unavailable {
        retry_after_ms: TRANSPORT_RETRY_MS,
    }
}

fn status_error(s: tonic::Status) -> ApiError {
    ApiError::from_status(&s).unwrap_or_else(|| match s.code() {
        tonic::Code::Unauthenticated => ApiError::Unauthenticated,
        tonic::Code::PermissionDenied => ApiError::PermissionDenied {
            scope: String::new(),
            path: None,
        },
        _ => unavailable(),
    })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

impl Server {
    /// Connects to the server at `url` (`http://host:port`). The mirror
    /// talks to the server, never a node, since only the server answers the
    /// admin service.
    pub async fn connect(url: &str, token: Option<String>) -> Result<Server, MirrorError> {
        let endpoint = Endpoint::parse(url);
        let Endpoint::Server(url) = &endpoint else {
            return Err(MirrorError::Config(
                "the mirror needs a server URL, not a node socket".to_owned(),
            ));
        };
        let admin = tonic::transport::Endpoint::from_shared(url.clone())
            .map_err(|_| MirrorError::Config("not a valid server URL".to_owned()))?
            .connect_timeout(CONNECT_TIMEOUT)
            .connect()
            .await
            .map_err(|_| MirrorError::Api(unavailable()))?;
        let mut remote = Remote::connect(endpoint.clone())
            .await
            .map_err(|e| match e {
                graft_client::Error::Api(e) => MirrorError::Api(e),
                e => MirrorError::Config(e.to_string()),
            })?;
        if let Some(t) = &token {
            remote = remote.with_token(t.clone());
        }
        Ok(Server {
            remote,
            admin,
            token,
        })
    }

    /// Opens `local:<path>` in-process, or connects to a server URL.
    pub async fn open(target: &str, token: Option<String>) -> Result<Api, MirrorError> {
        if let Some(path) = target.strip_prefix("local:") {
            if path.is_empty() {
                return Err(MirrorError::Config("local: needs a repository path".into()));
            }
            let repo =
                LocalRepo::open(Path::new(path)).map_err(|e| MirrorError::Config(e.to_string()))?;
            return Ok(Api::Local(Arc::new(repo)));
        }
        Ok(Api::Remote(Server::connect(target, token).await?))
    }

    fn admin(&self) -> AdminServiceClient<Channel> {
        AdminServiceClient::new(self.admin.clone())
            .send_compressed(CompressionEncoding::Gzip)
            .accept_compressed(CompressionEncoding::Gzip)
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES)
    }

    fn mirror(&self) -> MirrorServiceClient<Channel> {
        MirrorServiceClient::new(self.admin.clone())
            .send_compressed(CompressionEncoding::Gzip)
            .accept_compressed(CompressionEncoding::Gzip)
    }

    fn request<T>(&self, cx: &CallContext, msg: T) -> ApiResult<tonic::Request<T>> {
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
        let timeout = match cx.deadline_ms {
            Some(deadline) => {
                let left = deadline.saturating_sub(now_ms());
                if left <= 0 {
                    return Err(ApiError::Unavailable { retry_after_ms: 0 });
                }
                Duration::from_millis(left as u64)
            }
            None => DEFAULT_TIMEOUT,
        };
        req.set_timeout(timeout);
        Ok(req)
    }
}

fn wire(e: graft_proto::WireError) -> ApiError {
    ApiError::Rejected {
        policy: "protocol".to_owned(),
        detail: format!("malformed response: {e}"),
    }
}

impl AdminApi for Server {
    async fn create_repo(&self, cx: &CallContext, name: String) -> ApiResult<()> {
        let req = self.request(cx, pb::CreateRepoRequest { name })?;
        self.admin().create_repo(req).await.map_err(status_error)?;
        Ok(())
    }

    async fn import_changesets(
        &self,
        cx: &CallContext,
        req: ImportChangesets,
    ) -> ApiResult<Vec<Created>> {
        let req = self.request(cx, pb::ImportChangesetsRequest::from(req))?;
        let resp = self
            .admin()
            .import_changesets(req)
            .await
            .map_err(status_error)?;
        Vec::<Created>::try_from(resp.into_inner()).map_err(wire)
    }

    async fn gc(&self, cx: &CallContext, req: Gc) -> ApiResult<GcReport> {
        let req = self.request(cx, pb::GcRequest::from(req))?;
        let resp = self.admin().gc(req).await.map_err(status_error)?;
        Ok(GcReport::from(resp.into_inner()))
    }

    /// The mirror never mints tokens.
    async fn mint_token(&self, _cx: &CallContext, _req: MintToken) -> ApiResult<String> {
        Err(ApiError::PermissionDenied {
            scope: "mint".to_owned(),
            path: None,
        })
    }
}

impl ExportLog for Server {
    fn record_exports<'s>(
        &'s self,
        cx: &'s CallContext,
        repo: &'s str,
        exports: &'s [(ChangesetId, GitSha)],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ApiResult<()>> + Send + 's>> {
        Box::pin(async move {
            for chunk in exports.chunks(crate::MAX_RECORD_EXPORTS) {
                let req = self.request(
                    cx,
                    pb::RecordExportsRequest {
                        repo: repo.to_owned(),
                        exports: chunk
                            .iter()
                            .map(|(id, sha)| pb::ExportedCommit {
                                changeset: id.0.0.to_vec(),
                                commit: sha.to_hex(),
                            })
                            .collect(),
                    },
                )?;
                self.mirror()
                    .record_exports(req)
                    .await
                    .map_err(status_error)?;
            }
            Ok(())
        })
    }
}

macro_rules! delegate {
    ($($name:ident($req:ty) -> $out:ty;)*) => {
        $(
            async fn $name(&self, cx: &CallContext, req: $req) -> ApiResult<$out> {
                self.remote.$name(cx, req).await
            }
        )*
    };
}

impl ObjectApi for Server {
    delegate! {
        get_objects(GetObjects) -> Objects;
        has_objects(HasObjects) -> Vec<ObjectId>;
        put_objects(PutObjects) -> Vec<ObjectId>;
    }
}

impl RepoApi for Server {
    graft_proto::forward_repo_api!(delegate);
}

/// One `WorkspaceApi` method of [`Server`], every argument passed through.
macro_rules! delegate_workspace {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $out:ty;)*) => {
        $(
            async fn $name(&self, cx: &CallContext, $($arg: $ty),*) -> ApiResult<$out> {
                self.remote.$name(cx, $($arg),*).await
            }
        )*
    };
}

impl WorkspaceApi for Server {
    graft_proto::forward_workspace_api!(delegate_workspace);
}

macro_rules! api_delegate {
    ($($name:ident($req:ty) -> $out:ty;)*) => {
        $(
            async fn $name(&self, cx: &CallContext, req: $req) -> ApiResult<$out> {
                match self {
                    Api::Remote(server) => server.$name(cx, req).await,
                    Api::Local(repo) => repo.engine().$name(cx, req).await,
                }
            }
        )*
    };
}

impl ObjectApi for Api {
    api_delegate! {
        get_objects(GetObjects) -> Objects;
        has_objects(HasObjects) -> Vec<ObjectId>;
        put_objects(PutObjects) -> Vec<ObjectId>;
    }
}

impl RepoApi for Api {
    graft_proto::forward_repo_api!(api_delegate);
}

impl AdminApi for Api {
    async fn create_repo(&self, cx: &CallContext, name: String) -> ApiResult<()> {
        match self {
            Api::Remote(server) => server.create_repo(cx, name).await,
            Api::Local(repo) => repo.engine().create_repo(cx, name).await,
        }
    }
    async fn import_changesets(
        &self,
        cx: &CallContext,
        req: ImportChangesets,
    ) -> ApiResult<Vec<Created>> {
        match self {
            Api::Remote(server) => server.import_changesets(cx, req).await,
            Api::Local(repo) => repo.engine().import_changesets(cx, req).await,
        }
    }
    async fn gc(&self, cx: &CallContext, req: Gc) -> ApiResult<GcReport> {
        match self {
            Api::Remote(server) => server.gc(cx, req).await,
            Api::Local(repo) => repo.engine().gc(cx, req).await,
        }
    }
    async fn mint_token(&self, cx: &CallContext, req: MintToken) -> ApiResult<String> {
        match self {
            Api::Remote(server) => server.mint_token(cx, req).await,
            Api::Local(_) => Err(ApiError::unimplemented("mint_token")),
        }
    }
}

impl ExportLog for Api {
    fn record_exports<'s>(
        &'s self,
        cx: &'s CallContext,
        repo: &'s str,
        exports: &'s [(ChangesetId, graft_git::GitSha)],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ApiResult<()>> + Send + 's>> {
        match self {
            Api::Remote(server) => server.record_exports(cx, repo, exports),
            Api::Local(_) => Box::pin(async { Ok(()) }),
        }
    }
}
