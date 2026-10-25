//! `graft-node`: one cache daemon per host (`docs/design.md` §5.1, work item
//! G2.3).
//!
//! It serves every session on the host over a Unix socket, speaking the same
//! gRPC API as the server:
//!
//! - `GetObjects` is answered from a bounded on-disk cache (LRU by bytes) in
//!   the frame format the server stores and the wire carries. Misses are
//!   single-flighted ([`flight`]): concurrent callers missing the same object
//!   share one upstream fetch. Every frame is verified against its id when
//!   it is fetched and again when it is read from disk; a corrupt entry is
//!   discarded and fetched again.
//! - Every other call (repository, workspace and admin calls, `HasObjects`,
//!   `PutObjects`) is forwarded to the server unchanged with the caller's
//!   token ([`forward`]). Their answers are never cached.
//! - The caller's token is verified locally with the server's public key
//!   before a cached object is served ([`auth`]).
//! - Upstream calls are bounded and shed with `Unavailable` rather than
//!   queued without limit, and a circuit breaker fails calls fast while the
//!   server is unreachable ([`upstream`]). Connections, streams per
//!   connection, and objects in flight are bounded too.

pub mod config;
pub mod daemon;
pub mod forward;
pub mod metrics;
pub mod socket;

mod auth;
mod cache;
mod flight;
mod objects;
mod repos;
mod upstream;

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::task::JoinSet;

use axum::Router;
use axum::extract::Request as HttpRequest;
use axum::http::HeaderValue;
use axum::middleware::{self, Next};
use graft_auth::{Token, VerifyingKey};
use graft_proto::{PROTOCOL_VERSION, VERSION_HEADER, pb};
use tokio::net::UnixListener;
use tonic::codec::CompressionEncoding;

pub use config::NodeConfig;
pub use metrics::NodeMetrics;
pub use socket::Peer;

use crate::auth::Verifier;
use crate::cache::Cache;
use crate::flight::Flights;
use crate::forward::Forwarder;
use crate::objects::Objects;
use crate::upstream::{Upstream, UpstreamConfig};

/// A running node's state. Cheap to clone.
///
/// `GetObjects` answers and cache-miss fetches are started with
/// [`Node::track`]. Those tasks clone this node and hold the cache store
/// lock until they finish. [`Node::serve`] aborts them when the socket stops
/// and joins them before it returns, so the lock is free when `serve`
/// finishes (WI-2467).
#[derive(Clone)]
pub struct Node {
    objects: Objects,
    forwarder: Forwarder,
    metrics: NodeMetrics,
    max_connections: usize,
    max_streams: u32,
    tracked: Tracker,
}

/// Tasks spawned by the node that clone it (object answers, fetch leads).
///
/// [`Node::serve`] joins them, so their store handles are dropped before
/// `serve` returns (WI-2467).
#[derive(Clone)]
pub(crate) struct Tracker {
    set: Arc<Mutex<JoinSet<()>>>,
    stopping: Arc<AtomicBool>,
}

impl Tracker {
    fn new() -> Self {
        Self {
            set: Arc::new(Mutex::new(JoinSet::new())),
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Spawns `task`. After shutdown has begun, does not spawn.
    ///
    /// The registrar does not hold a [`Node`]. It takes the same lock
    /// [`Tracker::join`] takes before inserting, so a task cannot be missed
    /// by shutdown except during the gap closed by [`Tracker::join`]'s second
    /// pass.
    pub(crate) fn spawn<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let set = self.set.clone();
        let stopping = self.stopping.clone();
        tokio::spawn(async move {
            let mut set = set.lock().await;
            if stopping.load(Ordering::Acquire) {
                return;
            }
            set.spawn(task);
        });
    }

    /// Aborts every spawned task and waits until it has dropped.
    async fn join(&self) {
        for _ in 0..8 {
            {
                let mut set = self.set.lock().await;
                self.stopping.store(true, Ordering::Release);
                set.abort_all();
                while set.join_next().await.is_some() {}
            }
            tokio::task::yield_now().await;
        }
    }
}

impl Node {
    /// Opens the cache and sets up the upstream channel (connected lazily).
    /// Must be called inside a runtime.
    pub fn open(config: &NodeConfig, metrics: NodeMetrics) -> Result<Self, String> {
        config.check().map_err(|e| e.to_string())?;
        let key = VerifyingKey::from_bytes(&config.public_key().map_err(|e| e.to_string())?)
            .map_err(|_| "server_public_key is not a valid Ed25519 key".to_owned())?;
        let service_token = match &config.upstream_token_file {
            Some(path) => Some(read_token(path)?),
            None => None,
        };
        let cache = Arc::new(Cache::open(
            &config.cache_dir,
            config.max_bytes,
            config.min_free_bytes,
            config.max_repos,
            metrics.clone(),
        )?);
        let upstream = Upstream::new(
            &UpstreamConfig {
                url: config.upstream.clone(),
                max_calls: config.max_upstream_calls,
                max_waiting: config.max_upstream_waiting,
                wait: Duration::from_millis(config.upstream_wait_ms),
                connect_timeout: Duration::from_millis(config.upstream_connect_timeout_ms),
                timeout: Duration::from_millis(config.upstream_timeout_ms),
            },
            metrics.clone(),
        )?;
        let tracked = Tracker::new();
        let flights = Arc::new(Flights::new(
            config.max_inflight_objects,
            cache.clone(),
            upstream.clone(),
            service_token,
            metrics.clone(),
            tracked.clone(),
        ));
        Ok(Self {
            objects: Objects {
                verifier: Verifier::new(key),
                cache,
                flights,
                metrics: metrics.clone(),
                tasks: tracked.clone(),
            },
            forwarder: Forwarder {
                upstream,
                metrics: metrics.clone(),
            },
            metrics,
            max_connections: config.max_connections,
            max_streams: config.max_streams_per_connection,
            tracked,
        })
    }

    /// The metrics.
    pub fn metrics(&self) -> &NodeMetrics {
        &self.metrics
    }

    /// The gRPC surface: `GetObjects` from the cache, everything else
    /// forwarded, the protocol version stamped on every answer.
    pub fn router(&self) -> Router {
        let objects = pb::object_service_server::ObjectServiceServer::new(self.objects.clone())
            .accept_compressed(CompressionEncoding::Gzip)
            .max_encoding_message_size(flight::MAX_MESSAGE);
        let forwarder = self.forwarder.clone();
        let forward = tower::service_fn(move |req: HttpRequest| {
            let f = forwarder.clone();
            async move { Ok::<_, Infallible>(f.forward(req).await) }
        });
        Router::new()
            .route_service("/graft.v1.ObjectService/GetObjects", objects)
            .fallback_service(forward)
            .layer(middleware::from_fn(
                |req: HttpRequest, next: Next| async move {
                    let mut resp = next.run(req).await;
                    let version = HeaderValue::try_from(PROTOCOL_VERSION.to_string())
                        .expect("version is ASCII");
                    resp.headers_mut().insert(VERSION_HEADER, version);
                    resp
                },
            ))
    }

    /// Serves the socket until `shutdown` resolves.
    pub async fn serve(
        &self,
        listener: UnixListener,
        shutdown: impl Future<Output = ()> + Send,
    ) -> Result<(), String> {
        let (incoming, closer) =
            socket::incoming(listener, self.max_connections, self.metrics.clone());
        // The socket stops answering as soon as shutdown starts, not once the
        // open connections have drained: a restart may take the path at once.
        let shutdown = async move {
            shutdown.await;
            closer.close();
        };
        let served = tonic::transport::Server::builder()
            .max_concurrent_streams(Some(self.max_streams))
            .http2_keepalive_interval(Some(Duration::from_secs(30)))
            .add_routes(tonic::service::Routes::from(self.router()))
            .serve_with_incoming_shutdown(incoming, shutdown)
            .await
            .map_err(|e| format!("serve: {e}"));
        // GetObjects and fetch tasks hold Node clones. Join them before
        // returning so the cache lock is free (WI-2467).
        self.tracked.join().await;
        served
    }
}

/// Reads the service token file, which must be private to this user.
fn read_token(path: &std::path::Path) -> Result<Token, String> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() || meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{} must be a regular file with no group or other permissions",
            path.display()
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let token = text.trim();
    if token.is_empty() || token.len() > graft_auth::MAX_TOKEN_LEN {
        return Err(format!("{} does not hold a token", path.display()));
    }
    Ok(Token::from_encoded(token.to_owned()))
}
