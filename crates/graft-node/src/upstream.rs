//! The upstream server: one HTTP/2 channel, a bound on calls in flight with
//! load shedding, and a circuit breaker so that calls fail fast while the
//! upstream is unreachable (`docs/design.md` §5.1, §14).
//!
//! Strict tiering: the node never queues without limit. A caller waits for
//! an upstream slot only while fewer than `max_waiting` others are waiting,
//! and only for `wait`; otherwise it gets `Unavailable` at once. After a
//! transport failure the breaker opens for a backoff that doubles per
//! failure (with jitter, so the hosts of a fleet don't retry in step) and
//! every call in that window fails at once. The first call after it is the
//! probe: success closes the breaker, failure reopens it for longer.

use std::collections::hash_map::RandomState;
use std::future::Future;
use std::hash::{BuildHasher as _, Hasher as _};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use graft_proto::ApiError;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tonic::body::Body;
use tonic::codegen::http;
use tonic::transport::{Channel, Endpoint};
use tower::ServiceExt as _;

use crate::metrics::NodeMetrics;

/// The first breaker backoff.
const BACKOFF_MIN: Duration = Duration::from_millis(250);

/// The longest breaker backoff.
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// The retry hint when shedding for load.
const SHED_RETRY_MS: u64 = 500;

/// Bounds and timeouts of the upstream connection.
#[derive(Clone, Debug)]
pub(crate) struct UpstreamConfig {
    /// `http://host:port`.
    pub(crate) url: String,
    /// Calls in flight.
    pub(crate) max_calls: usize,
    /// Callers waiting for a slot.
    pub(crate) max_waiting: usize,
    /// The longest wait for a slot.
    pub(crate) wait: Duration,
    /// Connection timeout.
    pub(crate) connect_timeout: Duration,
    /// Per-call timeout.
    pub(crate) timeout: Duration,
}

/// The upstream: a cloneable handle on one channel.
#[derive(Clone)]
pub(crate) struct Upstream {
    channel: Channel,
    shared: Arc<Shared>,
}

struct Shared {
    slots: Arc<Semaphore>,
    waiting: AtomicUsize,
    max_waiting: usize,
    wait: Duration,
    timeout: Duration,
    breaker: Mutex<Breaker>,
    metrics: NodeMetrics,
}

#[derive(Default)]
struct Breaker {
    failures: u32,
    open_until: Option<Instant>,
}

/// A held upstream slot; dropping it frees the slot.
pub(crate) struct Slot(#[allow(dead_code)] OwnedSemaphorePermit);

impl Upstream {
    /// A lazily connected upstream. Must be called inside a runtime.
    pub(crate) fn new(config: &UpstreamConfig, metrics: NodeMetrics) -> Result<Self, String> {
        let channel = Endpoint::from_shared(config.url.clone())
            .map_err(|_| "upstream is not a valid URL".to_owned())?
            .connect_timeout(config.connect_timeout)
            .timeout(config.timeout)
            .tcp_nodelay(true)
            .http2_keep_alive_interval(Duration::from_secs(10))
            .keep_alive_timeout(Duration::from_secs(5))
            .connect_lazy();
        Ok(Self {
            channel,
            shared: Arc::new(Shared {
                slots: Arc::new(Semaphore::new(config.max_calls)),
                waiting: AtomicUsize::new(0),
                max_waiting: config.max_waiting,
                wait: config.wait,
                timeout: config.timeout,
                breaker: Mutex::new(Breaker::default()),
                metrics,
            }),
        })
    }

    /// The per-call timeout.
    pub(crate) fn timeout(&self) -> Duration {
        self.shared.timeout
    }

    /// Takes an upstream slot, or sheds: `Unavailable` at once while the
    /// breaker is open or the queue is full, or after the bounded wait.
    pub(crate) async fn slot(&self) -> Result<Slot, ApiError> {
        let s = &self.shared;
        self.breaker_check()?;
        if let Ok(p) = s.slots.clone().try_acquire_owned() {
            return Ok(Slot(p));
        }
        if s.waiting.fetch_add(1, Ordering::AcqRel) >= s.max_waiting {
            s.waiting.fetch_sub(1, Ordering::AcqRel);
            return Err(self.shed("queue", SHED_RETRY_MS));
        }
        let got = tokio::time::timeout(s.wait, s.slots.clone().acquire_owned()).await;
        s.waiting.fetch_sub(1, Ordering::AcqRel);
        match got {
            Ok(Ok(p)) => Ok(Slot(p)),
            _ => Err(self.shed("wait", SHED_RETRY_MS)),
        }
    }

    /// Records a shed and returns its error.
    pub(crate) fn shed(&self, reason: &str, retry_after_ms: u64) -> ApiError {
        self.shared.metrics.shed.with_label_values(&[reason]).inc();
        ApiError::Unavailable { retry_after_ms }
    }

    fn breaker_check(&self) -> Result<(), ApiError> {
        let b = self.shared.lock_breaker();
        match b.open_until {
            Some(until) => {
                let now = Instant::now();
                if now < until {
                    let ms = (until - now).as_millis() as u64;
                    drop(b);
                    return Err(self.shed("breaker", ms.max(1)));
                }
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Sends one HTTP request upstream. A transport failure opens the
    /// breaker and is `Unavailable`; any HTTP answer closes it.
    pub(crate) async fn send(
        &self,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, ApiError> {
        self.breaker_check()?;
        match self.channel.clone().oneshot(req).await {
            Ok(resp) => {
                self.shared.succeeded();
                Ok(resp)
            }
            Err(e) => {
                let retry = self.shared.failed();
                tracing::warn!(error = %e, retry_ms = retry, "upstream unreachable");
                Err(ApiError::Unavailable {
                    retry_after_ms: retry,
                })
            }
        }
    }
}

impl Shared {
    fn lock_breaker(&self) -> std::sync::MutexGuard<'_, Breaker> {
        self.breaker.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn succeeded(&self) {
        let mut b = self.lock_breaker();
        b.failures = 0;
        b.open_until = None;
    }

    /// Opens the breaker; returns its backoff in ms.
    fn failed(&self) -> u64 {
        let mut b = self.lock_breaker();
        b.failures = b.failures.saturating_add(1);
        let base = BACKOFF_MIN
            .saturating_mul(1u32 << (b.failures - 1).min(5))
            .min(BACKOFF_MAX);
        let jitter_ms =
            RandomState::new().build_hasher().finish() % (base.as_millis() as u64 / 2 + 1);
        let backoff = base + Duration::from_millis(jitter_ms);
        b.open_until = Some(Instant::now() + backoff);
        backoff.as_millis() as u64
    }
}

/// The upstream as a tower service, for the generated gRPC clients.
impl tower::Service<http::Request<Body>> for Upstream {
    type Response = http::Response<Body>;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            this.send(req)
                .await
                .map_err(|e| Box::new(tonic::Status::from(e)) as Self::Error)
        })
    }
}
