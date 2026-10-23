//! The shared server state, authentication, and the one path every call
//! takes into the engine (`docs/design.md` §12.2, §13).
//!
//! A call is authenticated here (bearer token, PASETO `v4.public`), checked
//! against the free-space floor when it writes, then run on its own task so
//! an engine operation that a later work item still has to build (its body
//! is `todo!`) answers `Unimplemented` instead of taking the connection down.
//! Scopes are checked by the engine, per operation.

use std::future::Future;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use graft_auth::{NoDenylist, SigningKey, Token, VerifyingKey};
use graft_engine::Engine;
use graft_proto::{ApiError, CallContext, PROTOCOL_VERSION};
use graft_store_fs::FsObjectStore;
use graft_store_sqlite::SqliteMetaStore;
use graft_telemetry::Telemetry;
use tracing::Instrument as _;

use crate::admission::Admission;
use crate::ci::CiExchange;
use crate::denylist::Denylist;
use crate::exports::Exports;
use crate::watch::Watches;

/// The engine over the production stores.
pub(crate) type ServerEngine = Engine<SqliteMetaStore, FsObjectStore>;

/// The header carrying the bearer token.
pub(crate) const AUTHORIZATION: &str = "authorization";

/// The header carrying the client's request id.
pub(crate) const REQUEST_ID: &str = "graft-request-id";

/// Longest request id kept; longer ones are replaced.
const MAX_REQUEST_ID: usize = 128;

/// Why a call failed.
#[derive(Debug)]
pub(crate) enum CallError {
    /// A typed API error.
    Api(ApiError),
    /// The operation is not built yet in this release.
    Unimplemented(&'static str),
    /// The operation failed unexpectedly (a bug); details are in the log.
    Internal,
}

impl From<ApiError> for CallError {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl From<CallError> for tonic::Status {
    fn from(e: CallError) -> Self {
        match e {
            CallError::Api(e) => e.into(),
            CallError::Unimplemented(op) => {
                tonic::Status::unimplemented(format!("{op} is not implemented in this release"))
            }
            CallError::Internal => tonic::Status::internal("internal error"),
        }
    }
}

/// Bytes buffered per budget permit.
const BUDGET_UNIT: u64 = 1024;

/// The bound on `PutObjects` buffering: a cap per call, and a budget shared
/// by every call in flight.
#[derive(Debug)]
pub(crate) struct PutBudget {
    /// Most frame bytes one call buffers.
    pub(crate) call_bytes: u64,
    /// How long a message waits for budget before the call is refused.
    pub(crate) wait: std::time::Duration,
    /// The longest gap between messages of one call.
    pub(crate) idle: std::time::Duration,
    shared: Arc<tokio::sync::Semaphore>,
}

impl PutBudget {
    /// A per-call cap and a shared budget, both in bytes. The budget is at
    /// least one call's cap, so a single call can always proceed.
    pub(crate) fn new(call_bytes: u64, shared_bytes: u64) -> Self {
        let permits = shared_bytes.max(call_bytes).div_ceil(BUDGET_UNIT);
        let permits = usize::try_from(permits)
            .unwrap_or(usize::MAX)
            .min(tokio::sync::Semaphore::MAX_PERMITS);
        Self {
            call_bytes,
            wait: std::time::Duration::from_secs(10),
            idle: std::time::Duration::from_secs(30),
            shared: Arc::new(tokio::sync::Semaphore::new(permits)),
        }
    }

    /// Sets the budget wait and the idle gap.
    pub(crate) fn with_waits(self, wait: std::time::Duration, idle: std::time::Duration) -> Self {
        Self { wait, idle, ..self }
    }

    /// Takes `bytes` of the shared budget, waiting while it is spent. The
    /// share comes back when the permit drops.
    pub(crate) async fn reserve(&self, bytes: u64) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let units = u32::try_from(bytes.div_ceil(BUDGET_UNIT)).ok()?;
        if units == 0 {
            return None;
        }
        self.shared.clone().acquire_many_owned(units).await.ok()
    }

    /// Unused budget, bytes (for tests).
    #[cfg(test)]
    pub(crate) fn available(&self) -> u64 {
        self.shared.available_permits() as u64 * BUDGET_UNIT
    }
}

/// An authenticated call: the caller's context, and what a later check
/// needs from its token (its expiry, and its `jti` for the denylist).
/// Derefs to the [`CallContext`].
#[derive(Clone, Debug)]
pub(crate) struct Caller {
    pub(crate) cx: CallContext,
    /// Token expiry, seconds since the epoch.
    pub(crate) exp_s: i64,
    /// Token id, checked against the denylist on writes.
    pub(crate) jti: String,
}

impl Deref for Caller {
    type Target = CallContext;

    fn deref(&self) -> &CallContext {
        &self.cx
    }
}

/// The server: engine, keys, admission, the denylist and telemetry.
pub(crate) struct App {
    pub(crate) engine: Arc<ServerEngine>,
    pub(crate) verifying: VerifyingKey,
    pub(crate) signing: SigningKey,
    pub(crate) admission: Admission,
    pub(crate) telemetry: Arc<Telemetry>,
    pub(crate) max_token_ttl_s: u64,
    pub(crate) watches: Watches,
    pub(crate) put_budget: PutBudget,
    pub(crate) denylist: Arc<Denylist>,
    /// The commits graft-mirror exported, which pinned CI exchanges check.
    pub(crate) exports: Arc<Exports>,
    /// The CI token exchange, when the config trusts an issuer.
    pub(crate) ci: Option<Arc<CiExchange>>,
    ready: AtomicBool,
    next_request: AtomicU64,
}

impl App {
    /// Builds the server state; it is not ready until [`App::set_ready`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        engine: Arc<ServerEngine>,
        signing: SigningKey,
        admission: Admission,
        telemetry: Arc<Telemetry>,
        max_token_ttl_s: u64,
        watches: Watches,
        put_budget: PutBudget,
        denylist: Denylist,
        exports: Arc<Exports>,
    ) -> Self {
        Self {
            engine,
            verifying: signing.verifying_key(),
            signing,
            admission,
            telemetry,
            max_token_ttl_s,
            watches,
            put_budget,
            denylist: Arc::new(denylist),
            exports,
            ci: None,
            ready: AtomicBool::new(false),
            next_request: AtomicU64::new(1),
        }
    }

    /// Turns on the CI token exchange.
    pub(crate) fn with_ci(mut self, ci: Option<Arc<CiExchange>>) -> Self {
        self.ci = ci;
        self
    }

    /// Refuses a write by a revoked token. A denylist that cannot be read
    /// refuses too: the server does not know the token is still good.
    pub(crate) async fn check_not_revoked(&self, jti: &str) -> Result<(), ApiError> {
        let deny = self.denylist.clone();
        let jti = jti.to_owned();
        match tokio::task::spawn_blocking(move || deny.is_revoked(&jti)).await {
            Ok(Ok(false)) => Ok(()),
            Ok(Ok(true)) => Err(ApiError::Unauthenticated),
            _ => {
                tracing::error!("denylist unreadable; refusing the write");
                Err(ApiError::Unavailable {
                    retry_after_ms: 1_000,
                })
            }
        }
    }

    /// Whether `/readyz` reports ready.
    pub(crate) fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Marks the server ready (serving) or draining.
    pub(crate) fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::Release);
    }

    /// Seconds since the epoch, for token expiry.
    pub(crate) fn now_s(&self) -> i64 {
        crate::mint::now_s()
    }

    /// Authenticates a call from its headers. `grpc` calls must also bind
    /// the protocol version; the JSON facade does not ask simple tools for
    /// one.
    pub(crate) fn authenticate<'a>(
        &self,
        header: impl Fn(&str) -> Option<&'a str>,
        grpc: bool,
    ) -> Result<Caller, ApiError> {
        if grpc {
            PROTOCOL_VERSION.accept(header(graft_proto::VERSION_HEADER))?;
        }
        let token = header(AUTHORIZATION)
            .and_then(bearer)
            .ok_or(ApiError::Unauthenticated)?;
        let claims = graft_auth::verify(
            &self.verifying,
            &Token::from_encoded(token.to_owned()),
            self.now_s(),
            &NoDenylist,
        )?;
        let request_id = match header(REQUEST_ID) {
            Some(id) if is_request_id(id) => id.to_owned(),
            _ => format!("srv-{}", self.next_request.fetch_add(1, Ordering::Relaxed)),
        };
        Ok(Caller {
            cx: CallContext {
                principal: claims.principal,
                request_id,
                deadline_ms: None,
            },
            exp_s: claims.exp,
            jti: claims.jti,
        })
    }

    /// Runs one engine call for `op` on its own task, inside a request span,
    /// recording its latency and outcome. Writes check the token against
    /// the denylist and pass the free-space floor first.
    pub(crate) async fn call<T, F, Fut>(
        &self,
        op: &'static str,
        repo: &str,
        caller: Caller,
        write: bool,
        f: F,
    ) -> Result<T, CallError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<ServerEngine>, CallContext) -> Fut,
        Fut: Future<Output = Result<T, ApiError>> + Send + 'static,
    {
        let span = tracing::info_span!(
            "rpc",
            op,
            repo,
            request_id = %caller.request_id,
            principal = %caller.principal.subject,
            session = caller.principal.session.as_deref().unwrap_or(""),
            work_item = caller.principal.work_item.as_deref().unwrap_or(""),
            tier = "server",
        );
        let started = Instant::now();
        let jti = caller.jti.clone();
        let cx = caller.cx;
        let result = async {
            if write {
                self.check_not_revoked(&jti).await?;
                self.admission.check_write(self.telemetry.metrics())?;
            }
            let task = tokio::spawn(f(self.engine.clone(), cx).in_current_span());
            match task.await {
                Ok(result) => result.map_err(CallError::Api),
                Err(e) if e.is_panic() => {
                    if is_todo(e.into_panic()) {
                        Err(CallError::Unimplemented(op))
                    } else {
                        tracing::error!("operation panicked");
                        Err(CallError::Internal)
                    }
                }
                Err(_) => {
                    tracing::error!("operation was cancelled");
                    Err(CallError::Internal)
                }
            }
        }
        .instrument(span.clone())
        .await;
        let outcome = match &result {
            Ok(_) => "ok",
            Err(CallError::Api(e)) => error_name(e),
            Err(CallError::Unimplemented(_)) => "unimplemented",
            Err(CallError::Internal) => "internal",
        };
        self.observe(op, outcome, started);
        span.in_scope(|| match &result {
            Ok(_) => tracing::debug!(outcome, "done"),
            Err(CallError::Api(e)) => tracing::info!(outcome, error = %e, "refused"),
            Err(_) => tracing::warn!(outcome, "failed"),
        });
        result
    }

    /// Records a call that failed before reaching the engine (bad
    /// credentials, a malformed request).
    pub(crate) fn refused(&self, op: &'static str, e: &ApiError) {
        self.observe(op, error_name(e), Instant::now());
        tracing::info!(op, outcome = error_name(e), error = %e, "refused");
    }

    pub(crate) fn observe(&self, op: &str, outcome: &str, started: Instant) {
        self.telemetry
            .metrics()
            .rpc_duration_seconds
            .with_label_values(&[op, "server", outcome])
            .observe(started.elapsed().as_secs_f64());
    }
}

/// The token in an `Authorization: Bearer <token>` value.
fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

/// A request id worth logging: short, printable ASCII, no spaces.
fn is_request_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_REQUEST_ID && id.bytes().all(|b| b.is_ascii_graphic())
}

/// Whether a panic payload is `todo!()`: an operation a later work item
/// builds.
fn is_todo(payload: Box<dyn std::any::Any + Send>) -> bool {
    let text = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied());
    text.is_some_and(|t| t.starts_with("not yet implemented"))
}

/// The metric `outcome` label for an error.
pub(crate) fn error_name(e: &ApiError) -> &'static str {
    match e {
        ApiError::NotFound { .. } => "not_found",
        ApiError::Collected { .. } => "collected",
        ApiError::InvalidPath { .. } => "invalid_path",
        ApiError::InvalidChange { .. } => "invalid_change",
        ApiError::UnknownParent { .. } => "unknown_parent",
        ApiError::MissingObjects { .. } => "missing_objects",
        ApiError::RefConflict { .. } => "ref_conflict",
        ApiError::Conflict { .. } => "conflict",
        ApiError::Contended { .. } => "contended",
        ApiError::Rejected { .. } => "rejected",
        ApiError::PermissionDenied { .. } => "permission_denied",
        ApiError::LeaseExpired { .. } => "lease_expired",
        ApiError::LimitExceeded { .. } => "limit_exceeded",
        ApiError::CorruptObject { .. } => "corrupt_object",
        ApiError::Obliterated { .. } => "obliterated",
        ApiError::Unavailable { .. } => "unavailable",
        ApiError::Unauthenticated => "unauthenticated",
        ApiError::IncompatibleProtocol { .. } => "incompatible_protocol",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_put_budget_is_shared_and_returned() {
        let b = PutBudget::new(4096, 8192);
        let one = b.reserve(4096).await;
        let two = b.reserve(4000).await;
        assert_eq!(b.available(), 0);
        let waiting = tokio::time::timeout(std::time::Duration::from_millis(50), b.reserve(1));
        assert!(
            waiting.await.is_err(),
            "a spent budget makes the caller wait"
        );
        drop(one);
        assert_eq!(b.available(), 4096);
        drop(two);
        assert!(b.reserve(0).await.is_none());
    }

    #[test]
    fn bearer_values() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer  abc "), Some("abc"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("Bearer "), None);
        assert_eq!(bearer("abc"), None);
    }

    #[test]
    fn request_ids() {
        assert!(is_request_id("r-1"));
        assert!(!is_request_id(""));
        assert!(!is_request_id("a b"));
        assert!(!is_request_id(&"x".repeat(MAX_REQUEST_ID + 1)));
    }

    #[test]
    fn todo_panics_are_recognised() {
        let payload = std::panic::catch_unwind(|| todo!("G1.10")).expect_err("panics");
        assert!(is_todo(payload));
        let payload = std::panic::catch_unwind(|| panic!("boom")).expect_err("panics");
        assert!(!is_todo(payload));
    }
}
