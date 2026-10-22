//! Just enough of the gRPC API for the crash suite's load: upload blobs,
//! create changesets, move refs by CAS, and read them back. Every call has
//! a deadline, so a killed server fails a call instead of hanging it.

use std::time::Duration;

use graft_model::{
    Author, AuthorKind, Change, ChangesetId, EntryKind, ObjectId, ObjectKind, RepoPath, Timestamp,
};
use graft_proto::pb;
use graft_proto::types::{
    Changesets, CreateChangeset, Created, PutObjects, RefKind, RefView, ResolveRef, UpdateRef,
};
use graft_proto::{ApiError, Frame, PROTOCOL_VERSION, VERSION_HEADER};
use tonic::codec::CompressionEncoding;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use super::proc::Token;

/// The deadline of one call.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Adds the bearer token, the protocol version and the deadline.
#[derive(Clone)]
struct Auth(String);

impl Interceptor for Auth {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        let md = req.metadata_mut();
        let mut v: tonic::metadata::MetadataValue<_> = format!("Bearer {}", self.0)
            .parse()
            .map_err(|_| tonic::Status::internal("token is not a header value"))?;
        v.set_sensitive(true);
        md.insert("authorization", v);
        md.insert(
            VERSION_HEADER,
            PROTOCOL_VERSION
                .to_string()
                .parse()
                .expect("version is ASCII"),
        );
        req.set_timeout(CALL_TIMEOUT);
        Ok(req)
    }
}

type Svc = InterceptedService<Channel, Auth>;

/// A failed call: the typed error, or a transport failure (the server died).
#[derive(Debug)]
pub(crate) enum CallError {
    Api(ApiError),
    Transport(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Api(e) => write!(f, "{e:?}"),
            CallError::Transport(t) => write!(f, "transport: {t}"),
        }
    }
}

fn err(s: tonic::Status) -> CallError {
    match ApiError::from_status(&s) {
        Some(e) => CallError::Api(e),
        None => CallError::Transport(format!("{:?}", s.code())),
    }
}

/// The clients.
pub(crate) struct Rpc {
    objects: pb::object_service_client::ObjectServiceClient<Svc>,
    repo: pb::repo_service_client::RepoServiceClient<Svc>,
    admin: pb::admin_service_client::AdminServiceClient<Svc>,
}

impl Rpc {
    /// Connects to `url` within the call deadline.
    pub(crate) async fn connect(url: &str, token: &Token) -> Result<Rpc, CallError> {
        let channel = tonic::transport::Endpoint::from_shared(url.to_owned())
            .map_err(|e| CallError::Transport(e.to_string()))?
            .connect_timeout(CALL_TIMEOUT)
            .connect()
            .await
            .map_err(|e| CallError::Transport(e.to_string()))?;
        let auth = Auth(token.expose().to_owned());
        let gz = CompressionEncoding::Gzip;
        Ok(Rpc {
            objects: pb::object_service_client::ObjectServiceClient::with_interceptor(
                channel.clone(),
                auth.clone(),
            )
            .send_compressed(gz)
            .accept_compressed(gz),
            repo: pb::repo_service_client::RepoServiceClient::with_interceptor(
                channel.clone(),
                auth.clone(),
            )
            .send_compressed(gz)
            .accept_compressed(gz),
            admin: pb::admin_service_client::AdminServiceClient::with_interceptor(channel, auth),
        })
    }

    pub(crate) async fn create_repo(&mut self, name: &str) -> Result<(), CallError> {
        self.admin
            .create_repo(pb::CreateRepoRequest { name: name.into() })
            .await
            .map(drop)
            .map_err(err)
    }

    pub(crate) async fn put_blobs(
        &mut self,
        repo: &str,
        blobs: &[Vec<u8>],
    ) -> Result<(), CallError> {
        let msg: pb::PutObjectsRequest = PutObjects {
            repo: repo.into(),
            blobs: blobs
                .iter()
                .map(|b| Frame::encode(ObjectKind::Blob, b))
                .collect(),
        }
        .into();
        self.objects
            .put_objects(tonic::codegen::tokio_stream::iter(vec![msg]))
            .await
            .map(drop)
            .map_err(err)
    }

    /// Creates a changeset putting `files` (path, bytes) on `parent`.
    pub(crate) async fn commit(
        &mut self,
        repo: &str,
        parent: Option<ChangesetId>,
        files: &[(String, Vec<u8>)],
        n: u64,
    ) -> Result<Created, CallError> {
        let changes = files
            .iter()
            .map(|(p, b)| Change::Put {
                path: RepoPath::new(p).expect("valid path"),
                kind: EntryKind::File,
                id: ObjectId::hash(ObjectKind::Blob, b),
            })
            .collect();
        let req = CreateChangeset {
            repo: repo.into(),
            parents: parent.into_iter().collect(),
            changes,
            author: Author {
                name: "Crash".into(),
                email: "crash@example.invalid".into(),
                kind: AuthorKind::Agent,
            },
            authored_at: Timestamp {
                ms: 1_700_000_000_000 + n as i64,
                offset_min: 0,
            },
            message: format!("crash load {n}"),
            metadata: Default::default(),
            inline_blobs: Vec::new(),
            workspace: None,
        };
        let out = self
            .repo
            .create_changeset(pb::CreateChangesetRequest::from(req))
            .await
            .map_err(err)?
            .into_inner();
        Created::try_from(out).map_err(|e| CallError::Transport(format!("bad Created: {e:?}")))
    }

    pub(crate) async fn update_ref(
        &mut self,
        repo: &str,
        name: &str,
        expected: Option<ChangesetId>,
        new: ChangesetId,
    ) -> Result<(), CallError> {
        self.repo
            .update_ref(pb::UpdateRefRequest::from(UpdateRef {
                repo: repo.into(),
                name: name.into(),
                expected,
                new,
                kind: RefKind::Line,
            }))
            .await
            .map(drop)
            .map_err(err)
    }

    /// The ref's target, `None` when it does not exist.
    pub(crate) async fn resolve(
        &mut self,
        repo: &str,
        name: &str,
    ) -> Result<Option<ChangesetId>, CallError> {
        match self
            .repo
            .resolve_ref(pb::ResolveRefRequest::from(ResolveRef {
                repo: repo.into(),
                name: name.into(),
            }))
            .await
        {
            Ok(v) => RefView::try_from(v.into_inner())
                .map(|v| Some(v.target))
                .map_err(|e| CallError::Transport(format!("bad RefView: {e:?}"))),
            Err(s) => match err(s) {
                CallError::Api(ApiError::NotFound { .. }) => Ok(None),
                e => Err(e),
            },
        }
    }

    /// The ids among `ids` the server does not have.
    pub(crate) async fn missing_changesets(
        &mut self,
        repo: &str,
        ids: &[ChangesetId],
    ) -> Result<Vec<ChangesetId>, CallError> {
        let mut missing = Vec::new();
        for chunk in ids.chunks(200) {
            let got = self
                .repo
                .get_changesets(pb::GetChangesetsRequest {
                    repo: repo.into(),
                    ids: chunk.iter().map(|i| i.0.0.to_vec()).collect(),
                })
                .await
                .map_err(err)?
                .into_inner();
            let got = Changesets::try_from(got)
                .map_err(|e| CallError::Transport(format!("bad Changesets: {e:?}")))?;
            missing.extend(got.missing);
            missing.extend(got.collected);
        }
        Ok(missing)
    }

    /// The blobs among `ids` the server does not have.
    pub(crate) async fn missing_objects(
        &mut self,
        repo: &str,
        ids: &[ObjectId],
    ) -> Result<Vec<ObjectId>, CallError> {
        let mut missing = Vec::new();
        for chunk in ids.chunks(500) {
            let got = self
                .objects
                .has_objects(pb::HasObjectsRequest {
                    repo: repo.into(),
                    ids: chunk.iter().map(|i| i.0.to_vec()).collect(),
                })
                .await
                .map_err(err)?
                .into_inner();
            // The answer lists the ids not stored.
            for raw in got.ids {
                let id: [u8; 32] = raw
                    .try_into()
                    .map_err(|_| CallError::Transport("bad object id".into()))?;
                missing.push(ObjectId(id));
            }
        }
        Ok(missing)
    }
}
