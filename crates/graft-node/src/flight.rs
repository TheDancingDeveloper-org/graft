//! Single-flight object fetches (`docs/design.md` §5.1).
//!
//! Every miss is keyed by `(repository, id)`. The first caller to miss a key
//! leads its fetch; every caller that misses it while the fetch is in flight
//! waits on the same answer. Thirty sessions cloning the same slice cause one
//! upstream fetch per object. Keys include the repository, so a caller never
//! shares another repository's fetch.
//!
//! A leader's fetch runs on its own task, so a caller that goes away does
//! not strand the others. It re-checks the cache first (a fetch that ended
//! between the caller's lookup and its join has already cached the object),
//! takes an upstream slot, and asks upstream for all of its keys in one
//! call. Each frame upstream returns is verified against its id before it is
//! cached or shared; a frame that fails is neither, and its waiters get
//! `CorruptObject`. Frames for ids it did not ask for are dropped. Ids the
//! upstream neither returns nor lists are reported deferred, so the caller
//! asks again. Negative answers (`missing`) are shared with the waiters of
//! this fetch only and never cached: an object can appear later.
//!
//! The number of keys in flight is bounded; a miss past the bound is shed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use graft_auth::Token;
use graft_model::ObjectId;
use graft_proto::frame::{HEADER_LEN, MAX_FRAME_PAYLOAD};
use graft_proto::pb;
use graft_proto::pb::object_service_client::ObjectServiceClient;
use graft_proto::{ApiError, PROTOCOL_VERSION, VERSION_HEADER};
use tokio::sync::watch;
use tonic::codec::CompressionEncoding;
use tonic::metadata::MetadataValue;

use crate::auth::REQUEST_ID;
use crate::cache::{self, Cache};
use crate::metrics::NodeMetrics;
use crate::upstream::Upstream;

/// The largest message the node decodes from upstream: one maximal frame
/// plus framing.
pub(crate) const MAX_MESSAGE: usize = 80 << 20;

/// The most frame bytes the node accepts from one upstream `GetObjects`:
/// the server's 64 MiB answer bound plus one frame that may straddle it.
const MAX_ANSWER_BYTES: u64 = (64 << 20) + MAX_FRAME_PAYLOAD + HEADER_LEN as u64;

/// The retry hint when the in-flight bound sheds a miss.
const INFLIGHT_RETRY_MS: u64 = 250;

/// What one key's fetch came to.
#[derive(Clone, Debug)]
pub(crate) enum Outcome {
    /// The verified frame bytes.
    Found(Arc<Vec<u8>>),
    /// Upstream does not have it.
    Missing,
    /// Not answered this time; ask again.
    Deferred,
    /// The fetch failed.
    Failed(ApiError),
}

/// A waiter on one key.
pub(crate) type Waiter = watch::Receiver<Option<Outcome>>;

type Key = (String, ObjectId);

/// The fetches in flight.
pub(crate) struct Flights {
    map: Mutex<HashMap<Key, Waiter>>,
    max: usize,
    cache: Arc<Cache>,
    upstream: Upstream,
    /// A service token for fetches, else the leader's caller's token.
    service_token: Option<Token>,
    metrics: NodeMetrics,
    next_request: AtomicU64,
    /// Joins fetch leads when the node stops (WI-2467).
    tasks: crate::Tracker,
}

impl Flights {
    pub(crate) fn new(
        max: usize,
        cache: Arc<Cache>,
        upstream: Upstream,
        service_token: Option<Token>,
        metrics: NodeMetrics,
        tasks: crate::Tracker,
    ) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            max,
            cache,
            upstream,
            service_token,
            metrics,
            next_request: AtomicU64::new(0),
            tasks,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Waiter>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Joins (or leads) the fetch of each of `ids` in `repo`, in order.
    /// `token` is the caller's, used when the node has no service token.
    pub(crate) fn join(
        self: &Arc<Self>,
        repo: &str,
        ids: &[ObjectId],
        token: &Token,
    ) -> Vec<Waiter> {
        let mut waiters = Vec::with_capacity(ids.len());
        let mut led = Vec::new();
        {
            let mut map = self.lock();
            for &id in ids {
                let key = (repo.to_owned(), id);
                if let Some(w) = map.get(&key) {
                    self.metrics.base.singleflight_shared_total.inc();
                    waiters.push(w.clone());
                } else if map.len() >= self.max {
                    let e = self.upstream.shed("inflight", INFLIGHT_RETRY_MS);
                    waiters.push(watch::channel(Some(Outcome::Failed(e))).1);
                } else {
                    let (tx, rx) = watch::channel(None);
                    map.insert(key.clone(), rx.clone());
                    waiters.push(rx);
                    led.push((id, key, tx));
                }
            }
            self.metrics.inflight_objects.set(map.len() as i64);
        }
        if !led.is_empty() {
            let token = self.service_token.clone().unwrap_or_else(|| token.clone());
            let pending = led
                .into_iter()
                .map(|(id, key, tx)| {
                    let flights = self.clone();
                    (id, Answer { flights, key, tx })
                })
                .collect();
            let lead = Lead {
                flights: self.clone(),
                repo: repo.to_owned(),
                pending,
                fills: tokio::task::JoinSet::new(),
                filling: Arc::new(AtomicU64::new(0)),
            };
            // The lead holds `Flights` (and the cache) until the node joins it.
            self.tasks.spawn(lead.run(token));
        }
        waiters
    }
}

/// Waits for a key's outcome.
pub(crate) async fn wait(mut w: Waiter) -> Outcome {
    match w.wait_for(Option::is_some).await {
        Ok(v) => v.clone().unwrap_or(Outcome::Deferred),
        // The leader is gone without answering (it always answers on drop,
        // so only a panic gets here).
        Err(_) => Outcome::Failed(ApiError::Unavailable { retry_after_ms: 0 }),
    }
}

/// Frame bytes a leader holds in verification and cache writes at once
/// (one larger frame may go alone).
const FILL_BYTES: u64 = 8 << 20;

/// One key's answer. [`Answer::publish`] gives the waiters their outcome;
/// the key leaves the in-flight map only when the answer is dropped, so a
/// caller that misses meanwhile joins this answer instead of starting a
/// second fetch. A found frame's answer is dropped once it is cached. An
/// answer dropped unpublished tells its waiters `Unavailable`.
struct Answer {
    flights: Arc<Flights>,
    key: Key,
    tx: watch::Sender<Option<Outcome>>,
}

impl Answer {
    fn publish(&self, outcome: Outcome) {
        self.tx.send_replace(Some(outcome));
    }
}

impl Drop for Answer {
    fn drop(&mut self) {
        let unsent = self.tx.borrow().is_none();
        if unsent {
            self.publish(Outcome::Failed(ApiError::Unavailable { retry_after_ms: 0 }));
        }
        let mut map = self.flights.lock();
        map.remove(&self.key);
        self.flights.metrics.inflight_objects.set(map.len() as i64);
    }
}

/// One leader's keys and the cache fills it has started.
struct Lead {
    flights: Arc<Flights>,
    repo: String,
    pending: HashMap<ObjectId, Answer>,
    fills: tokio::task::JoinSet<()>,
    /// Frame bytes in `fills`.
    filling: Arc<AtomicU64>,
}

impl Lead {
    fn resolve(&mut self, id: ObjectId, outcome: Outcome) {
        if let Some(answer) = self.pending.remove(&id) {
            answer.publish(outcome);
        }
    }

    fn resolve_rest(&mut self, outcome: Outcome) {
        for (_, answer) in self.pending.drain() {
            answer.publish(outcome.clone());
        }
    }

    async fn run(mut self, token: Token) {
        let timeout = self.flights.upstream.timeout();
        if tokio::time::timeout(timeout, self.fetch(token))
            .await
            .is_err()
        {
            self.resolve_rest(Outcome::Failed(ApiError::Unavailable { retry_after_ms: 0 }));
        }
        // Ids upstream neither returned nor listed: ask again.
        self.resolve_rest(Outcome::Deferred);
        while self.fills.join_next().await.is_some() {}
    }

    async fn fetch(&mut self, token: Token) {
        let flights = self.flights.clone();
        // A fetch that just finished may have cached some of these. Check
        // presence for all, then read the few present one at a time.
        let ids: Vec<ObjectId> = self.pending.keys().copied().collect();
        let repo = self.repo.clone();
        let present = cache::blocking(&flights.cache, move |c| {
            let present = c.has(&repo, &ids);
            ids.into_iter()
                .zip(present)
                .filter_map(|(id, p)| p.then_some(id))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        for id in present {
            let repo = self.repo.clone();
            if let Some(Some(bytes)) =
                cache::blocking(&flights.cache, move |c| c.get(&repo, id)).await
            {
                self.resolve(id, Outcome::Found(Arc::new(bytes)));
            }
        }
        if self.pending.is_empty() {
            return;
        }

        let _slot = match flights.upstream.slot().await {
            Ok(slot) => slot,
            Err(e) => return self.resolve_rest(Outcome::Failed(e)),
        };
        let asked: Vec<ObjectId> = self.pending.keys().copied().collect();
        let mut req = tonic::Request::new(pb::GetObjectsRequest {
            repo: self.repo.clone(),
            ids: asked.iter().map(|id| id.0.to_vec()).collect(),
        });
        let seq = flights.next_request.fetch_add(1, Ordering::Relaxed);
        let md = req.metadata_mut();
        md.insert(
            VERSION_HEADER,
            MetadataValue::try_from(PROTOCOL_VERSION.to_string()).expect("version is ASCII"),
        );
        if let Ok(v) = MetadataValue::try_from(format!("node-{seq}")) {
            md.insert(REQUEST_ID, v);
        }
        match MetadataValue::try_from(format!("Bearer {}", token.expose())) {
            Ok(mut v) => {
                v.set_sensitive(true);
                md.insert("authorization", v);
            }
            Err(_) => return self.resolve_rest(Outcome::Failed(ApiError::Unauthenticated)),
        }
        req.set_timeout(flights.upstream.timeout());

        let mut client = ObjectServiceClient::new(flights.upstream.clone())
            .accept_compressed(CompressionEncoding::Gzip)
            .max_decoding_message_size(MAX_MESSAGE);
        let mut stream = match client.get_objects(req).await {
            Ok(resp) => resp.into_inner(),
            Err(status) => return self.resolve_rest(Outcome::Failed(api_error(&status))),
        };
        let mut bytes = 0u64;
        loop {
            let page = match stream.message().await {
                Ok(Some(page)) => page,
                Ok(None) => break,
                Err(status) => return self.resolve_rest(Outcome::Failed(api_error(&status))),
            };
            bytes += page.found.iter().map(|f| f.frame.len() as u64).sum::<u64>();
            if bytes > MAX_ANSWER_BYTES {
                flights.metrics.upstream_protocol_errors.inc();
                break;
            }
            for found in page.found {
                if let Some(answer) = self.expected(&found.id) {
                    self.accept(answer, found.frame).await;
                }
            }
            for (list, outcome) in [
                (page.missing, Outcome::Missing),
                (page.deferred, Outcome::Deferred),
            ] {
                for raw in list {
                    if let Some(answer) = self.expected(&raw) {
                        answer.publish(outcome.clone());
                    }
                }
            }
        }
    }

    /// The answer for `raw`, if it is an id this fetch asked for and has
    /// not had answered. Anything else breaks the protocol and is dropped.
    fn expected(&mut self, raw: &[u8]) -> Option<Answer> {
        let answer = <[u8; 32]>::try_from(raw)
            .ok()
            .and_then(|id| self.pending.remove(&ObjectId(id)));
        if answer.is_none() {
            self.flights.metrics.upstream_protocol_errors.inc();
        }
        answer
    }

    /// Verifies a fetched frame on the blocking pool; a good one is given
    /// to its waiters and then cached, a bad one is neither.
    async fn accept(&mut self, answer: Answer, frame: Vec<u8>) {
        let m = &self.flights.metrics;
        m.upstream_objects.inc();
        m.base.fetch_objects_total.inc();
        let codec = match frame.get(5) {
            Some(1) => "gzip",
            _ => "raw",
        };
        m.base
            .fetch_bytes_total
            .with_label_values(&[crate::metrics::TIER, codec])
            .inc_by(frame.len() as u64);
        let len = frame.len() as u64;
        while !self.fills.is_empty() && self.filling.load(Ordering::Acquire) + len > FILL_BYTES {
            self.fills.join_next().await;
        }
        self.filling.fetch_add(len, Ordering::AcqRel);
        let filling = self.filling.clone();
        let cache = self.flights.cache.clone();
        self.fills.spawn_blocking(move || {
            let _done = Release(filling, len);
            let (repo, id) = (&answer.key.0, answer.key.1);
            if cache::verify(id, &frame).is_err() {
                answer
                    .flights
                    .metrics
                    .corrupt
                    .with_label_values(&["upstream"])
                    .inc();
                tracing::error!(repo = %repo, id = %id.to_hex(), "upstream sent a corrupt object");
                return answer.publish(Outcome::Failed(ApiError::CorruptObject { id }));
            }
            let frame = Arc::new(frame);
            answer.publish(Outcome::Found(frame.clone()));
            cache.store(repo, id, &frame);
        });
    }
}

/// Gives a fill's bytes back to its leader's budget.
struct Release(Arc<AtomicU64>, u64);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.fetch_sub(self.1, Ordering::AcqRel);
    }
}

/// The API error a status carries, else `Unavailable`.
pub(crate) fn api_error(status: &tonic::Status) -> ApiError {
    ApiError::from_status(status).unwrap_or(ApiError::Unavailable {
        retry_after_ms: 1_000,
    })
}
