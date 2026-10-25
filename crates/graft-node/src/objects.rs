//! `GetObjects` from the cache (`docs/design.md` §7.2, §7.3).
//!
//! The caller's token is verified locally and must grant reads of the
//! repository (or `objects:read`). Hits stream straight from the cache;
//! misses join single-flight fetches. The answer has the server's bounds: at
//! most [`MAX_IDS`] distinct ids are served and the rest deferred, and once
//! the frames sent reach [`MAX_ANSWER_BYTES`] the remaining ids are deferred
//! too. Frames stream in pages of about [`PAGE_BYTES`] through a one-page
//! channel, so a slow client holds the node back instead of the node
//! buffering the answer. If an object can't be had (upstream down, shed,
//! corrupt upstream), the call ends with that error.
//!
//! The other object calls (`HasObjects`, `PutObjects`) are forwarded: what
//! the server holds can change (uploads, collection), so the node never
//! answers them from its cache.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use graft_model::ObjectId;
use graft_proto::pb;
use graft_proto::types::GetObjects;
use graft_proto::{ApiError, WireError};
use tokio::sync::mpsc;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::auth::{self, Verifier};
use crate::cache::{self, Cache};
use crate::flight::{self, Flights, Outcome};
use crate::metrics::NodeMetrics;
use crate::repos;

/// Distinct ids one call serves; the rest are deferred.
pub(crate) const MAX_IDS: usize = 1024;

/// Frame bytes after which the remaining ids are deferred.
pub(crate) const MAX_ANSWER_BYTES: u64 = 64 << 20;

/// Frame bytes per stream message; a larger frame travels alone.
const PAGE_BYTES: usize = 1 << 20;

/// Ids looked up and joined at a time.
const BATCH: usize = 64;

/// The node's object service.
#[derive(Clone)]
pub(crate) struct Objects {
    pub(crate) verifier: Verifier,
    pub(crate) cache: Arc<Cache>,
    pub(crate) flights: Arc<Flights>,
    pub(crate) metrics: NodeMetrics,
    /// Joins the answer task when the node stops (WI-2467).
    pub(crate) tasks: crate::Tracker,
}

#[tonic::async_trait]
impl pb::object_service_server::ObjectService for Objects {
    type GetObjectsStream = ReceiverStream<Result<pb::Objects, Status>>;

    async fn get_objects(
        &self,
        request: Request<pb::GetObjectsRequest>,
    ) -> Result<Response<Self::GetObjectsStream>, Status> {
        let started = Instant::now();
        let refuse = |e: ApiError| {
            self.metrics.rpc("GetObjects", "refused", started);
            Status::from(e)
        };
        let caller = self.verifier.verify(request.metadata()).map_err(refuse)?;
        let peer = request.extensions().get::<crate::socket::Peer>().cloned();
        let req = GetObjects::try_from(request.into_inner()).map_err(|e: WireError| {
            self.metrics.rpc("GetObjects", "refused", started);
            Status::from(e)
        })?;
        if !repos::is_name(&req.repo) {
            return Err(refuse(ApiError::NotFound {
                what: "repository".into(),
            }));
        }
        auth::may_read_objects(&caller.cx, &req.repo).map_err(refuse)?;
        tracing::debug!(
            op = "GetObjects",
            repo = %req.repo,
            request_id = %caller.cx.request_id,
            principal = %caller.cx.principal.subject,
            session = caller.cx.principal.session.as_deref().unwrap_or(""),
            work_item = caller.cx.principal.work_item.as_deref().unwrap_or(""),
            peer_uid = peer.as_ref().map(|p| p.uid),
            peer_pid = peer.as_ref().and_then(|p| p.pid),
            ids = req.ids.len(),
            tier = "node",
            "rpc"
        );

        let mut seen = HashSet::new();
        let mut ids: Vec<ObjectId> = req.ids.into_iter().filter(|id| seen.insert(*id)).collect();
        let deferred = ids.split_off(ids.len().min(MAX_IDS));
        let (tx, rx) = mpsc::channel(1);
        let this = self.clone();
        // Holds `this` (and the cache lock) until the node joins it.
        self.tasks.spawn(async move {
            let outcome = this
                .answer(req.repo, ids, deferred, caller.token, &tx)
                .await;
            this.metrics.rpc("GetObjects", outcome, started);
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn has_objects(
        &self,
        _request: Request<pb::HasObjectsRequest>,
    ) -> Result<Response<pb::ObjectIds>, Status> {
        Err(Status::internal("HasObjects is forwarded, not routed here"))
    }

    async fn put_objects(
        &self,
        _request: Request<Streaming<pb::PutObjectsRequest>>,
    ) -> Result<Response<pb::ObjectIds>, Status> {
        Err(Status::internal("PutObjects is forwarded, not routed here"))
    }
}

type Tx = mpsc::Sender<Result<pb::Objects, Status>>;

/// Pages of found frames.
struct Pages<'a> {
    tx: &'a Tx,
    page: pb::Objects,
    bytes: usize,
}

impl Pages<'_> {
    /// Adds a frame, sending the page when it is full. False if the client
    /// has gone.
    async fn push(&mut self, id: ObjectId, frame: Vec<u8>) -> bool {
        if self.bytes > 0 && self.bytes + frame.len() > PAGE_BYTES && !self.flush().await {
            return false;
        }
        self.bytes += frame.len();
        self.page.found.push(pb::FoundObject {
            id: id.0.to_vec(),
            frame,
        });
        true
    }

    async fn flush(&mut self) -> bool {
        if self.page.found.is_empty() {
            return true;
        }
        self.bytes = 0;
        let page = std::mem::take(&mut self.page);
        self.tx.send(Ok(page)).await.is_ok()
    }
}

/// What one id came to.
enum Step {
    Frame(Vec<u8>),
    Missing,
    Deferred,
    Fail(ApiError),
}

impl Objects {
    /// Reads one cached frame on the blocking pool.
    async fn read(&self, repo: &str, id: ObjectId) -> Option<Vec<u8>> {
        let repo = repo.to_owned();
        cache::blocking(&self.cache, move |c| c.get(&repo, id))
            .await
            .flatten()
    }

    /// Waits for a fetch's outcome.
    async fn fetched(waiter: flight::Waiter) -> Step {
        match flight::wait(waiter).await {
            Outcome::Found(bytes) => {
                Step::Frame(Arc::try_unwrap(bytes).unwrap_or_else(|shared| (*shared).clone()))
            }
            Outcome::Missing => Step::Missing,
            Outcome::Deferred => Step::Deferred,
            Outcome::Failed(e) => Step::Fail(e),
        }
    }

    /// Streams the answer; returns the outcome label.
    ///
    /// Per batch, presence is checked first (no reads), the misses join
    /// their fetches together, and then each id is answered in order: a hit
    /// is read (and verified) only when its turn comes, so a call holds at
    /// most one cached frame at a time. An entry that vanished or failed
    /// verification since the presence check is fetched on its own.
    async fn answer(
        &self,
        repo: String,
        ids: Vec<ObjectId>,
        mut deferred: Vec<ObjectId>,
        token: graft_auth::Token,
        tx: &Tx,
    ) -> &'static str {
        let mut missing = Vec::new();
        let mut sent = 0u64;
        let mut pages = Pages {
            tx,
            page: pb::Objects::default(),
            bytes: 0,
        };
        let mut batches = ids.chunks(BATCH);
        while let Some(batch) = batches.next() {
            if sent >= MAX_ANSWER_BYTES {
                deferred.extend(batch);
                deferred.extend(batches.by_ref().flatten());
                break;
            }
            let wanted = batch.to_vec();
            let r = repo.clone();
            let Some(present) = cache::blocking(&self.cache, move |c| c.has(&r, &wanted)).await
            else {
                let _ = tx
                    .send(Err(ApiError::Unavailable { retry_after_ms: 0 }.into()))
                    .await;
                return "error";
            };
            let misses: Vec<ObjectId> = batch
                .iter()
                .zip(&present)
                .filter(|(_, p)| !**p)
                .map(|(id, _)| *id)
                .collect();
            let mut waiters = self.flights.join(&repo, &misses, &token).into_iter();
            for (&id, present) in batch.iter().zip(present) {
                let hit = match present {
                    true => self.read(&repo, id).await,
                    false => None,
                };
                self.metrics.lookup(hit.is_some());
                let step = match hit {
                    Some(bytes) => Step::Frame(bytes),
                    None => {
                        let waiter = match present {
                            false => waiters.next().expect("one waiter per miss"),
                            true => self.flights.join(&repo, &[id], &token).remove(0),
                        };
                        Self::fetched(waiter).await
                    }
                };
                let frame = match step {
                    Step::Frame(frame) => frame,
                    Step::Missing => {
                        missing.push(id);
                        continue;
                    }
                    Step::Deferred => {
                        deferred.push(id);
                        continue;
                    }
                    Step::Fail(e) => {
                        let _ = tx.send(Err(e.into())).await;
                        return "error";
                    }
                };
                if sent >= MAX_ANSWER_BYTES {
                    deferred.push(id);
                    continue;
                }
                sent += frame.len() as u64;
                if !pages.push(id, frame).await {
                    return "gone";
                }
            }
        }
        if !pages.flush().await {
            return "gone";
        }
        let last = pb::Objects {
            found: Vec::new(),
            missing: missing.into_iter().map(|id| id.0.to_vec()).collect(),
            deferred: deferred.into_iter().map(|id| id.0.to_vec()).collect(),
        };
        let _ = tx.send(Ok(last)).await;
        "ok"
    }
}
