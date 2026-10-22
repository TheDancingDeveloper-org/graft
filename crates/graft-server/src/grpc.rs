//! The gRPC services (`graft.v1`), each a thin adapter: authenticate,
//! convert the message at the boundary, call the engine through
//! [`App::call`], convert the answer back.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use graft_model::ChangesetId;
use graft_proto::pb;
use graft_proto::types::*;
use graft_proto::{AdminApi as _, ApiError, CallContext, Frame, ObjectApi as _, RepoApi as _};
use graft_proto::{WireError, WorkspaceApi as _};
use tokio::sync::mpsc;
use tonic::codec::CompressionEncoding;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::app::{App, CallError};
use crate::mint;

/// Frame bytes per `GetObjects` stream message. A single frame larger than
/// this travels alone.
const PAGE_BYTES: usize = 4 << 20;

/// The largest message the server decodes: one maximal blob frame plus
/// framing.
pub(crate) const MAX_MESSAGE: usize = 80 << 20;

/// How often a following `Watch` polls the journal.
const WATCH_POLL: Duration = Duration::from_millis(100);

/// Decodes a wire message into its Rust type at the boundary.
fn wire<T, U>(m: U) -> Result<T, Status>
where
    T: TryFrom<U>,
    T::Error: Into<BoundaryError>,
{
    T::try_from(m).map_err(|e| e.into().0)
}

/// A conversion failure, as the status it travels under.
struct BoundaryError(Status);

impl From<WireError> for BoundaryError {
    fn from(e: WireError) -> Self {
        Self(e.into())
    }
}

impl From<Infallible> for BoundaryError {
    fn from(e: Infallible) -> Self {
        match e {}
    }
}

fn cid(id: ChangesetId) -> Vec<u8> {
    id.0.0.to_vec()
}

fn opt_cid(b: Option<Vec<u8>>, field: &'static str) -> Result<Option<ChangesetId>, Status> {
    b.map(|b| req_cid(b, field)).transpose()
}

fn req_cid(b: Vec<u8>, field: &'static str) -> Result<ChangesetId, Status> {
    let bytes: [u8; 32] = b.try_into().map_err(|_| {
        Status::from(WireError {
            field,
            reason: "not a 32-byte id",
        })
    })?;
    Ok(ChangesetId(graft_model::ObjectId(bytes)))
}

fn workspace_ref(w: Option<pb::WorkspaceRef>) -> Result<WorkspaceRef, Status> {
    w.map(Into::into).ok_or_else(|| {
        WireError {
            field: "workspace",
            reason: "required",
        }
        .into()
    })
}

/// The gRPC services over one [`App`].
#[derive(Clone)]
pub(crate) struct Services {
    app: Arc<App>,
}

impl Services {
    pub(crate) fn new(app: Arc<App>) -> Self {
        Self { app }
    }

    /// The services, gzip enabled both ways (`docs/design.md` §6.3), as
    /// tonic routes.
    pub(crate) fn routes(self) -> tonic::service::Routes {
        let gz = CompressionEncoding::Gzip;
        let objects = pb::object_service_server::ObjectServiceServer::new(self.clone())
            .accept_compressed(gz)
            .send_compressed(gz)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE);
        let repo = pb::repo_service_server::RepoServiceServer::new(self.clone())
            .accept_compressed(gz)
            .send_compressed(gz)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE);
        let workspaces = pb::workspace_service_server::WorkspaceServiceServer::new(self.clone())
            .accept_compressed(gz)
            .send_compressed(gz);
        let admin = pb::admin_service_server::AdminServiceServer::new(self)
            .accept_compressed(gz)
            .send_compressed(gz)
            .max_decoding_message_size(MAX_MESSAGE);
        tonic::service::Routes::new(objects)
            .add_service(repo)
            .add_service(workspaces)
            .add_service(admin)
    }

    /// Authenticates a gRPC call.
    fn cx<T>(&self, req: &Request<T>, op: &'static str) -> Result<CallContext, Status> {
        let md = req.metadata();
        self.app
            .authenticate(|k| md.get(k).and_then(|v| v.to_str().ok()), true)
            .map_err(|e| {
                self.app.refused(op, &e);
                e.into()
            })
    }
}

/// One unary call: authenticate, decode, run on the engine, encode.
macro_rules! unary {
    ($self:ident, $request:ident, $op:literal, $write:expr, $ty:ty,
     |$e:ident, $cx:ident, $req:ident| $body:expr, $out:expr) => {{
        let cx = $self.cx(&$request, $op)?;
        let $req: $ty = wire($request.into_inner())?;
        let repo = $req.repo.clone();
        let result = $self
            .app
            .call($op, &repo, cx, $write, move |$e, $cx| async move { $body })
            .await
            .map_err(Status::from)?;
        Ok(Response::new($out(result)))
    }};
}

#[tonic::async_trait]
impl pb::object_service_server::ObjectService for Services {
    type GetObjectsStream = ReceiverStream<Result<pb::Objects, Status>>;

    async fn get_objects(
        &self,
        request: Request<pb::GetObjectsRequest>,
    ) -> Result<Response<Self::GetObjectsStream>, Status> {
        let cx = self.cx(&request, "GetObjects")?;
        let req: GetObjects = wire(request.into_inner())?;
        let repo = req.repo.clone();
        let objects = self
            .app
            .call("GetObjects", &repo, cx, false, move |e, cx| async move {
                e.get_objects(&cx, req).await
            })
            .await
            .map_err(Status::from)?;
        let pages = paginate(objects);
        let (tx, rx) = mpsc::channel(pages.len().max(1));
        for page in pages {
            // The channel holds every page, so this never waits.
            let _ = tx.try_send(Ok(page));
        }
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn has_objects(
        &self,
        request: Request<pb::HasObjectsRequest>,
    ) -> Result<Response<pb::ObjectIds>, Status> {
        unary!(
            self,
            request,
            "HasObjects",
            false,
            HasObjects,
            |e, cx, req| e.has_objects(&cx, req).await,
            pb::ObjectIds::from
        )
    }

    async fn put_objects(
        &self,
        request: Request<Streaming<pb::PutObjectsRequest>>,
    ) -> Result<Response<pb::ObjectIds>, Status> {
        let cx = self.cx(&request, "PutObjects")?;
        let config = self.app.engine.config();
        let (max_ids, max_bytes) = (config.max_ids as usize, config.max_put_bytes);
        let mut stream = request.into_inner();
        let mut req = PutObjects::default();
        let mut bytes = 0u64;
        let mut first = true;
        while let Some(message) = stream.message().await? {
            let part: PutObjects = wire(message)?;
            if first {
                req.repo = part.repo;
                first = false;
            } else if part.repo != req.repo && !part.repo.is_empty() {
                return Err(WireError {
                    field: "repo",
                    reason: "differs between messages",
                }
                .into());
            }
            bytes += part.blobs.iter().map(|f| f.body.len() as u64).sum::<u64>();
            req.blobs.extend(part.blobs);
            // Bound what is buffered; the engine enforces the same limits.
            if req.blobs.len() > max_ids {
                return Err(limit("ids", max_ids as u64));
            }
            if bytes > max_bytes {
                return Err(limit("put-bytes", max_bytes));
            }
        }
        let repo = req.repo.clone();
        let ids = self
            .app
            .call("PutObjects", &repo, cx, true, move |e, cx| async move {
                e.put_objects(&cx, req).await
            })
            .await
            .map_err(Status::from)?;
        Ok(Response::new(ids.into()))
    }
}

fn limit(name: &str, max: u64) -> Status {
    ApiError::LimitExceeded {
        limit: name.to_owned(),
        max,
    }
    .into()
}

/// Splits a `GetObjects` answer into stream messages of about
/// [`PAGE_BYTES`] each; the last carries `missing` and `deferred`.
fn paginate(objects: Objects) -> Vec<pb::Objects> {
    let mut pages = Vec::new();
    let mut page = Vec::new();
    let mut bytes = 0usize;
    for (id, frame) in objects.found {
        let len = frame.body.len();
        if !page.is_empty() && bytes + len > PAGE_BYTES {
            pages.push(found_page(std::mem::take(&mut page)));
            bytes = 0;
        }
        bytes += len;
        page.push((id, frame));
    }
    let mut last = found_page(page);
    last.missing = objects.missing.iter().map(|id| id.0.to_vec()).collect();
    last.deferred = objects.deferred.iter().map(|id| id.0.to_vec()).collect();
    pages.push(last);
    pages
}

fn found_page(found: Vec<(graft_model::ObjectId, Frame)>) -> pb::Objects {
    Objects {
        found,
        ..Objects::default()
    }
    .into()
}

#[tonic::async_trait]
impl pb::repo_service_server::RepoService for Services {
    type WatchStream = ReceiverStream<Result<pb::JournalPage, Status>>;

    async fn create_changeset(
        &self,
        request: Request<pb::CreateChangesetRequest>,
    ) -> Result<Response<pb::Created>, Status> {
        unary!(
            self,
            request,
            "CreateChangeset",
            true,
            CreateChangeset,
            |e, cx, req| e.create_changeset(&cx, req).await,
            pb::Created::from
        )
    }

    async fn get_changesets(
        &self,
        request: Request<pb::GetChangesetsRequest>,
    ) -> Result<Response<pb::Changesets>, Status> {
        unary!(
            self,
            request,
            "GetChangesets",
            false,
            GetChangesets,
            |e, cx, req| e.get_changesets(&cx, req).await,
            pb::Changesets::from
        )
    }

    async fn resolve_ref(
        &self,
        request: Request<pb::ResolveRefRequest>,
    ) -> Result<Response<pb::RefView>, Status> {
        unary!(
            self,
            request,
            "ResolveRef",
            false,
            ResolveRef,
            |e, cx, req| e.resolve_ref(&cx, req).await,
            pb::RefView::from
        )
    }

    async fn list_refs(
        &self,
        request: Request<pb::ListRefsRequest>,
    ) -> Result<Response<pb::RefViews>, Status> {
        unary!(
            self,
            request,
            "ListRefs",
            false,
            ListRefs,
            |e, cx, req| e.list_refs(&cx, req).await,
            pb::RefViews::from
        )
    }

    async fn update_ref(
        &self,
        request: Request<pb::UpdateRefRequest>,
    ) -> Result<Response<pb::RefView>, Status> {
        unary!(
            self,
            request,
            "UpdateRef",
            true,
            UpdateRef,
            |e, cx, req| e.update_ref(&cx, req).await,
            pb::RefView::from
        )
    }

    async fn stat(&self, request: Request<pb::PathAt>) -> Result<Response<pb::EntryView>, Status> {
        unary!(
            self,
            request,
            "Stat",
            false,
            PathAt,
            |e, cx, req| e.stat(&cx, req).await,
            pb::EntryView::from
        )
    }

    async fn list_tree(
        &self,
        request: Request<pb::ListTreeRequest>,
    ) -> Result<Response<pb::Entries>, Status> {
        unary!(
            self,
            request,
            "ListTree",
            false,
            ListTree,
            |e, cx, req| e.list_tree(&cx, req).await,
            pb::Entries::from
        )
    }

    async fn read_file(
        &self,
        request: Request<pb::PathAt>,
    ) -> Result<Response<pb::FileContent>, Status> {
        unary!(
            self,
            request,
            "ReadFile",
            false,
            PathAt,
            |e, cx, req| e.read_file(&cx, req).await,
            pb::FileContent::from
        )
    }

    async fn log(&self, request: Request<pb::LogRequest>) -> Result<Response<pb::History>, Status> {
        unary!(
            self,
            request,
            "Log",
            false,
            Log,
            |e, cx, req| e.log(&cx, req).await,
            pb::History::from
        )
    }

    async fn is_ancestor(&self, request: Request<pb::Pair>) -> Result<Response<pb::Bool>, Status> {
        unary!(
            self,
            request,
            "IsAncestor",
            false,
            Pair,
            |e, cx, req| e.is_ancestor(&cx, req).await,
            |value| pb::Bool { value }
        )
    }

    async fn merge_base(
        &self,
        request: Request<pb::Pair>,
    ) -> Result<Response<pb::OptionalChangeset>, Status> {
        unary!(
            self,
            request,
            "MergeBase",
            false,
            Pair,
            |e, cx, req| e.merge_base(&cx, req).await,
            |id: Option<ChangesetId>| pb::OptionalChangeset { id: id.map(cid) }
        )
    }

    async fn diff(
        &self,
        request: Request<pb::DiffRequest>,
    ) -> Result<Response<pb::DiffResult>, Status> {
        unary!(
            self,
            request,
            "Diff",
            false,
            Diff,
            |e, cx, req| e.diff(&cx, req).await,
            pb::DiffResult::from
        )
    }

    async fn blame(
        &self,
        request: Request<pb::BlameRequest>,
    ) -> Result<Response<pb::BlameResult>, Status> {
        unary!(
            self,
            request,
            "Blame",
            false,
            Blame,
            |e, cx, req| e.blame(&cx, req).await,
            pb::BlameResult::from
        )
    }

    async fn rebase(
        &self,
        request: Request<pb::RebaseRequest>,
    ) -> Result<Response<pb::ChangesetRef>, Status> {
        unary!(
            self,
            request,
            "Rebase",
            true,
            Rebase,
            |e, cx, req| e.rebase(&cx, req).await,
            |id| pb::ChangesetRef { id: cid(id) }
        )
    }

    async fn revert(
        &self,
        request: Request<pb::RevertRequest>,
    ) -> Result<Response<pb::ChangesetRef>, Status> {
        unary!(
            self,
            request,
            "Revert",
            true,
            Revert,
            |e, cx, req| e.revert(&cx, req).await,
            |id| pb::ChangesetRef { id: cid(id) }
        )
    }

    async fn land(
        &self,
        request: Request<pb::LandRequest>,
    ) -> Result<Response<pb::Landed>, Status> {
        unary!(
            self,
            request,
            "Land",
            true,
            Land,
            |e, cx, req| e.land(&cx, req).await,
            pb::Landed::from
        )
    }

    async fn attest(
        &self,
        request: Request<pb::AttestRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        unary!(
            self,
            request,
            "Attest",
            true,
            Attest,
            |e, cx, req| e.attest(&cx, req).await,
            |()| pb::Empty {}
        )
    }

    /// Streams journal pages from `from_lsn`. With `wait_ms` 0 the stream
    /// ends once it has caught up; otherwise it follows the journal and ends
    /// after `wait_ms` with nothing new.
    async fn watch(
        &self,
        request: Request<pb::WatchRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        let cx = self.cx(&request, "Watch")?;
        let req: Watch = wire(request.into_inner())?;
        let app = self.app.clone();
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            let wait = Duration::from_millis(req.wait_ms.into());
            let mut from = req.from_lsn.max(1);
            let mut idle_since = Instant::now();
            loop {
                let mut page_req = req.clone();
                page_req.from_lsn = from;
                page_req.wait_ms = 0;
                let repo = page_req.repo.clone();
                let page = app
                    .call("Watch", &repo, cx.clone(), false, move |e, cx| async move {
                        e.watch(&cx, page_req).await
                    })
                    .await;
                let page = match page {
                    Ok(page) => page,
                    Err(e) => {
                        let _ = tx.send(Err(e.into())).await;
                        return;
                    }
                };
                let advanced = page.next_lsn > from;
                from = page.next_lsn;
                if advanced {
                    idle_since = Instant::now();
                    if tx.send(Ok(page.into())).await.is_err() {
                        return;
                    }
                    continue;
                }
                if idle_since.elapsed() >= wait {
                    let _ = tx.send(Ok(page.into())).await;
                    return;
                }
                tokio::select! {
                    () = tokio::time::sleep(WATCH_POLL) => {}
                    () = tx.closed() => return,
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

#[tonic::async_trait]
impl pb::workspace_service_server::WorkspaceService for Services {
    async fn open_workspace(
        &self,
        request: Request<pb::OpenWorkspaceRequest>,
    ) -> Result<Response<pb::WorkspaceView>, Status> {
        unary!(
            self,
            request,
            "OpenWorkspace",
            true,
            OpenWorkspace,
            |e, cx, req| e.open_workspace(&cx, req).await,
            pb::WorkspaceView::from
        )
    }

    async fn get_workspace(
        &self,
        request: Request<pb::WorkspaceRef>,
    ) -> Result<Response<pb::WorkspaceView>, Status> {
        unary!(
            self,
            request,
            "GetWorkspace",
            false,
            WorkspaceRef,
            |e, cx, req| e.get_workspace(&cx, req).await,
            pb::WorkspaceView::from
        )
    }

    async fn list_workspaces(
        &self,
        request: Request<pb::ListWorkspacesRequest>,
    ) -> Result<Response<pb::WorkspaceViews>, Status> {
        let cx = self.cx(&request, "ListWorkspaces")?;
        let r = request.into_inner();
        let page: Page = r.page.map(Into::into).unwrap_or_default();
        let repo = r.repo;
        let out = self
            .app
            .call(
                "ListWorkspaces",
                &repo.clone(),
                cx,
                false,
                move |e, cx| async move { e.list_workspaces(&cx, repo, page).await },
            )
            .await
            .map_err(Status::from)?;
        Ok(Response::new(out.into()))
    }

    async fn renew_lease(
        &self,
        request: Request<pb::RenewLeaseRequest>,
    ) -> Result<Response<pb::WorkspaceView>, Status> {
        let cx = self.cx(&request, "RenewLease")?;
        let r = request.into_inner();
        let ws = workspace_ref(r.workspace)?;
        let lease_ms = r.lease_ms;
        let repo = ws.repo.clone();
        let out = self
            .app
            .call("RenewLease", &repo, cx, true, move |e, cx| async move {
                e.renew_lease(&cx, ws, lease_ms).await
            })
            .await
            .map_err(Status::from)?;
        Ok(Response::new(out.into()))
    }

    async fn set_head(
        &self,
        request: Request<pb::SetHeadRequest>,
    ) -> Result<Response<pb::WorkspaceView>, Status> {
        let cx = self.cx(&request, "SetHead")?;
        let req: SetHead = wire(request.into_inner())?;
        let repo = req.workspace.repo.clone();
        let out = self
            .app
            .call("SetHead", &repo, cx, true, move |e, cx| async move {
                e.set_head(&cx, req).await
            })
            .await
            .map_err(Status::from)?;
        Ok(Response::new(out.into()))
    }

    async fn sync(
        &self,
        request: Request<pb::SyncRequest>,
    ) -> Result<Response<pb::Synced>, Status> {
        let cx = self.cx(&request, "Sync")?;
        let r = request.into_inner();
        let ws = workspace_ref(r.workspace)?;
        let onto = opt_cid(r.onto, "onto")?;
        let repo = ws.repo.clone();
        let out = self
            .app
            .call("Sync", &repo, cx, true, move |e, cx| async move {
                e.sync(&cx, ws, onto).await
            })
            .await
            .map_err(Status::from)?;
        Ok(Response::new(out.into()))
    }

    async fn stack(
        &self,
        request: Request<pb::StackRequest>,
    ) -> Result<Response<pb::WorkspaceView>, Status> {
        let cx = self.cx(&request, "Stack")?;
        let r = request.into_inner();
        let ws = workspace_ref(r.workspace)?;
        let op: StackOp = wire(r.op.ok_or_else(|| {
            Status::from(WireError {
                field: "op",
                reason: "required",
            })
        })?)?;
        let repo = ws.repo.clone();
        let out = self
            .app
            .call("Stack", &repo, cx, true, move |e, cx| async move {
                e.stack(&cx, ws, op).await
            })
            .await
            .map_err(Status::from)?;
        Ok(Response::new(out.into()))
    }

    async fn close_workspace(
        &self,
        request: Request<pb::CloseWorkspaceRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        let cx = self.cx(&request, "CloseWorkspace")?;
        let r = request.into_inner();
        let ws = workspace_ref(r.workspace)?;
        let keep = r.keep_drafts;
        let repo = ws.repo.clone();
        self.app
            .call("CloseWorkspace", &repo, cx, true, move |e, cx| async move {
                e.close_workspace(&cx, ws, keep).await
            })
            .await
            .map_err(Status::from)?;
        Ok(Response::new(pb::Empty {}))
    }
}

#[tonic::async_trait]
impl pb::admin_service_server::AdminService for Services {
    async fn create_repo(
        &self,
        request: Request<pb::CreateRepoRequest>,
    ) -> Result<Response<pb::Empty>, Status> {
        let cx = self.cx(&request, "CreateRepo")?;
        let name = request.into_inner().name;
        self.app
            .call(
                "CreateRepo",
                &name.clone(),
                cx,
                true,
                move |e, cx| async move { e.create_repo(&cx, name).await },
            )
            .await
            .map_err(Status::from)?;
        Ok(Response::new(pb::Empty {}))
    }

    async fn import_changesets(
        &self,
        request: Request<pb::ImportChangesetsRequest>,
    ) -> Result<Response<pb::CreatedList>, Status> {
        unary!(
            self,
            request,
            "ImportChangesets",
            true,
            ImportChangesets,
            |e, cx, req| e.import_changesets(&cx, req).await,
            pb::CreatedList::from
        )
    }

    async fn gc(&self, request: Request<pb::GcRequest>) -> Result<Response<pb::GcReport>, Status> {
        unary!(
            self,
            request,
            "Gc",
            true,
            Gc,
            |e, cx, req| e.gc(&cx, req).await,
            pb::GcReport::from
        )
    }

    async fn mint_token(
        &self,
        request: Request<pb::MintTokenRequest>,
    ) -> Result<Response<pb::Token>, Status> {
        let cx = self.cx(&request, "MintToken")?;
        let req: MintToken = wire(request.into_inner())?;
        let token = mint::mint_for(
            &self.app.signing,
            &cx.principal,
            req,
            self.app.max_token_ttl_s,
            self.app.now_s(),
        )
        .map_err(|e| {
            self.app.refused("MintToken", &e);
            Status::from(CallError::Api(e))
        })?;
        tracing::info!(op = "MintToken", principal = %cx.principal.subject, "minted a token");
        Ok(Response::new(pb::Token {
            token: token.expose().to_owned(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_model::{ObjectId, ObjectKind};

    #[test]
    fn pages_split_by_bytes_and_the_last_carries_misses() {
        // Incompressible, so each frame stays over half a page.
        let mut state = 1u64;
        let big: Vec<u8> = (0..PAGE_BYTES / 2 + 1)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                (state >> 56) as u8
            })
            .collect();
        let found: Vec<_> = (0..3u8)
            .map(|i| {
                let mut payload = big.clone();
                payload[0] = i;
                let frame = Frame::encode(ObjectKind::Blob, &payload);
                (ObjectId::hash(ObjectKind::Blob, &payload), frame)
            })
            .collect();
        let pages = paginate(Objects {
            found,
            missing: vec![ObjectId([1; 32])],
            deferred: vec![ObjectId([2; 32])],
        });
        assert_eq!(pages.iter().map(|p| p.found.len()).sum::<usize>(), 3);
        assert!(pages.len() >= 2, "{} pages", pages.len());
        let last = pages.last().expect("a page");
        assert_eq!(last.missing.len(), 1);
        assert_eq!(last.deferred.len(), 1);
        assert!(
            pages[..pages.len() - 1]
                .iter()
                .all(|p| p.missing.is_empty())
        );
    }

    #[test]
    fn empty_answer_is_one_page() {
        assert_eq!(paginate(Objects::default()).len(), 1);
    }
}
