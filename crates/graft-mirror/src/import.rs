//! `graft-mirror import <git-dir> --repo <name>` (`docs/design.md` §11.1):
//! create the repository, import with the fidelity check, then set the line
//! refs and create the tag refs, each by compare-and-swap.
//!
//! Re-running is safe: commits the mirror DB already holds are skipped, a
//! line that is already at its tip is left alone, and a tag that already
//! names its changeset is too.

use std::future::Future;
use std::time::Duration;

use graft_git::import::{ImportConfig, ImportReport, import};
use graft_git::map::MirrorMap;
use graft_model::ChangesetId;
use graft_proto::api::{AdminApi, ObjectApi, RepoApi};
use graft_proto::types::RefKind;
use graft_proto::{ApiError, CallContext};

use crate::{MirrorError, current_ref, move_ref, tag_ref};

/// How many times one import API call is tried, including the first.
const MAX_ATTEMPTS: u32 = 8;

/// The longest one hinted wait is honoured for. A longer hint is capped,
/// and the total of the waits is bounded by the same budget.
const MAX_WAIT: Duration = Duration::from_secs(30);

/// Waits out a retry hint. The real one sleeps; tests record the delay
/// and return at once, so no assertion depends on wall time.
pub trait Sleeper: Send + Sync {
    /// Waits `delay`.
    fn sleep(&self, delay: Duration) -> impl Future<Output = ()> + Send;
}

/// `tokio::time::sleep`.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioSleeper;

impl Sleeper for TokioSleeper {
    async fn sleep(&self, delay: Duration) {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }
}

/// Retries an import API call that answers `Unavailable`, after the hinted
/// delay (capped), until [`MAX_ATTEMPTS`] or [`MAX_WAIT`] is spent. Any
/// other error returns at once. `create_repo` and the import writes are
/// idempotent, so a retry after a call that did land is safe.
async fn retry<T, F, Fut, S>(sleeper: &S, mut call: F) -> Result<T, ApiError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ApiError>>,
    S: Sleeper,
{
    let mut waited = Duration::ZERO;
    let mut last = ApiError::Unavailable { retry_after_ms: 0 };
    for attempt in 0..MAX_ATTEMPTS {
        match call().await {
            Ok(v) => return Ok(v),
            Err(ApiError::Unavailable { retry_after_ms }) => {
                last = ApiError::Unavailable { retry_after_ms };
                if attempt + 1 == MAX_ATTEMPTS {
                    break;
                }
                let hint = Duration::from_millis(retry_after_ms);
                let delay = hint.min(MAX_WAIT).min(MAX_WAIT.saturating_sub(waited));
                if delay.is_zero() && !hint.is_zero() {
                    break;
                }
                sleeper.sleep(delay).await;
                waited = waited.saturating_add(delay);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

/// What an import did beyond `graft-git`'s report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportOutcome {
    /// `graft-git`'s report: commits, blobs, the line and tag tips.
    pub report: ImportReport,
    /// Lines whose ref this run created or moved.
    pub lines_moved: Vec<String>,
    /// Tag refs this run created.
    pub tags_created: Vec<String>,
    /// Tags already in graft naming another changeset (tags are immutable),
    /// and the changeset git names.
    pub tags_conflicting: Vec<(String, ChangesetId)>,
}

/// Imports `config.git_dir` into `config.repo` and sets its refs.
pub async fn import_repo<A, M>(
    api: &A,
    cx: &CallContext,
    map: &M,
    config: &ImportConfig,
) -> Result<ImportOutcome, MirrorError>
where
    A: AdminApi + ObjectApi + RepoApi,
    M: MirrorMap,
{
    import_repo_waiting(api, cx, map, config, &TokioSleeper).await
}

/// [`import_repo`] with the wait between `Unavailable` retries injected.
pub async fn import_repo_waiting<A, M, S>(
    api: &A,
    cx: &CallContext,
    map: &M,
    config: &ImportConfig,
    sleeper: &S,
) -> Result<ImportOutcome, MirrorError>
where
    A: AdminApi + ObjectApi + RepoApi,
    M: MirrorMap,
    S: Sleeper,
{
    let api = Waiting {
        inner: api,
        sleeper,
    };
    api.create_repo(cx, config.repo.clone()).await?;
    let report = import(&api, cx, map, config).await?;
    let mut out = ImportOutcome::default();
    for (line, id) in &report.lines {
        let current = current_ref(&api, cx, &config.repo, line).await?;
        if current != Some(*id) {
            move_ref(&api, cx, &config.repo, line, current, *id, RefKind::Line).await?;
            out.lines_moved.push(line.clone());
        }
    }
    for (tag, id) in &report.tags {
        let name = tag_ref(tag);
        match current_ref(&api, cx, &config.repo, &name).await? {
            Some(existing) if existing == *id => {}
            Some(_) => out.tags_conflicting.push((tag.clone(), *id)),
            None => match move_ref(&api, cx, &config.repo, &name, None, *id, RefKind::Tag).await {
                Ok(()) => out.tags_created.push(tag.clone()),
                // Created concurrently: report it as conflicting rather than
                // guess which is right.
                Err(ApiError::RefConflict { .. }) => out.tags_conflicting.push((tag.clone(), *id)),
                Err(e) => return Err(e.into()),
            },
        }
    }
    out.report = report;
    Ok(out)
}

/// The import API with `Unavailable` retried on the calls import makes.
/// Other methods are the trait defaults; import never calls them.
struct Waiting<'a, A, S> {
    inner: &'a A,
    sleeper: &'a S,
}

impl<A, S> Waiting<'_, A, S>
where
    A: AdminApi + ObjectApi + RepoApi,
    S: Sleeper,
{
    async fn again<T, F, Fut>(&self, call: F) -> Result<T, ApiError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, ApiError>>,
    {
        retry(self.sleeper, call).await
    }
}

impl<A, S> AdminApi for Waiting<'_, A, S>
where
    A: AdminApi + ObjectApi + RepoApi,
    S: Sleeper,
{
    async fn create_repo(&self, cx: &CallContext, name: String) -> Result<(), ApiError> {
        self.again(|| self.inner.create_repo(cx, name.clone()))
            .await
    }

    async fn import_changesets(
        &self,
        cx: &CallContext,
        req: graft_proto::types::ImportChangesets,
    ) -> Result<Vec<graft_proto::types::Created>, ApiError> {
        self.again(|| self.inner.import_changesets(cx, req.clone()))
            .await
    }

    async fn gc(
        &self,
        cx: &CallContext,
        req: graft_proto::types::Gc,
    ) -> Result<graft_proto::types::GcReport, ApiError> {
        self.inner.gc(cx, req).await
    }

    async fn mint_token(
        &self,
        cx: &CallContext,
        req: graft_proto::types::MintToken,
    ) -> Result<String, ApiError> {
        self.inner.mint_token(cx, req).await
    }
}

impl<A, S> ObjectApi for Waiting<'_, A, S>
where
    A: AdminApi + ObjectApi + RepoApi,
    S: Sleeper,
{
    async fn get_objects(
        &self,
        cx: &CallContext,
        req: graft_proto::types::GetObjects,
    ) -> Result<graft_proto::types::Objects, ApiError> {
        self.inner.get_objects(cx, req).await
    }

    async fn has_objects(
        &self,
        cx: &CallContext,
        req: graft_proto::types::HasObjects,
    ) -> Result<Vec<graft_model::ObjectId>, ApiError> {
        self.again(|| self.inner.has_objects(cx, req.clone())).await
    }

    async fn put_objects(
        &self,
        cx: &CallContext,
        req: graft_proto::types::PutObjects,
    ) -> Result<Vec<graft_model::ObjectId>, ApiError> {
        self.again(|| self.inner.put_objects(cx, req.clone())).await
    }
}

/// One `RepoApi` method of [`Waiting`]: the reads and the ref move the
/// import retries go through [`Waiting::again`]; the rest pass straight
/// through.
macro_rules! waiting {
    ($($name:ident($req:ty) -> $out:ty;)*) => {$(
        async fn $name(&self, cx: &CallContext, req: $req) -> Result<$out, ApiError> {
            waiting!(@call self, cx, req, $name)
        }
    )*};
    (@call $self:ident, $cx:ident, $req:ident, get_changesets) => {
        waiting!(@again $self, $cx, $req, get_changesets)
    };
    (@call $self:ident, $cx:ident, $req:ident, resolve_ref) => {
        waiting!(@again $self, $cx, $req, resolve_ref)
    };
    (@call $self:ident, $cx:ident, $req:ident, update_ref) => {
        waiting!(@again $self, $cx, $req, update_ref)
    };
    (@call $self:ident, $cx:ident, $req:ident, $name:ident) => {
        $self.inner.$name($cx, $req).await
    };
    (@again $self:ident, $cx:ident, $req:ident, $name:ident) => {
        $self.again(|| $self.inner.$name($cx, $req.clone())).await
    };
}

impl<A, S> RepoApi for Waiting<'_, A, S>
where
    A: AdminApi + ObjectApi + RepoApi,
    S: Sleeper,
{
    graft_proto::forward_repo_api!(waiting);
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    struct Virtual {
        slept: Mutex<Vec<Duration>>,
    }

    impl Sleeper for Virtual {
        async fn sleep(&self, delay: Duration) {
            self.slept.lock().unwrap().push(delay);
        }
    }

    struct Gate {
        /// `Unavailable` answers before success. `u32::MAX` never succeeds.
        left: AtomicU32,
        calls: AtomicU32,
        error: Option<ApiError>,
    }

    impl Gate {
        fn next(&self) -> Result<(), ApiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(e) = &self.error {
                return Err(e.clone());
            }
            let left = self.left.load(Ordering::SeqCst);
            if left == 0 {
                return Ok(());
            }
            if left != u32::MAX {
                self.left.fetch_sub(1, Ordering::SeqCst);
            }
            Err(ApiError::Unavailable {
                retry_after_ms: 1_000,
            })
        }
    }

    #[tokio::test]
    async fn retries_unavailable_then_succeeds() {
        let gate = Gate {
            left: AtomicU32::new(3),
            calls: AtomicU32::new(0),
            error: None,
        };
        let clock = Virtual {
            slept: Mutex::new(Vec::new()),
        };
        retry(&clock, || async { gate.next() }).await.unwrap();
        assert_eq!(gate.calls.load(Ordering::SeqCst), 4);
        assert_eq!(
            *clock.slept.lock().unwrap(),
            vec![Duration::from_millis(1_000); 3]
        );
    }

    #[tokio::test]
    async fn permanent_unavailable_stops_at_the_bound() {
        let gate = Gate {
            left: AtomicU32::new(u32::MAX),
            calls: AtomicU32::new(0),
            error: None,
        };
        let clock = Virtual {
            slept: Mutex::new(Vec::new()),
        };
        let err = retry(&clock, || async { gate.next() }).await.unwrap_err();
        assert!(matches!(
            err,
            ApiError::Unavailable {
                retry_after_ms: 1_000
            }
        ));
        assert_eq!(gate.calls.load(Ordering::SeqCst), MAX_ATTEMPTS);
        // Seven waits of 1s stay inside the 30s budget.
        assert_eq!(
            clock.slept.lock().unwrap().len(),
            (MAX_ATTEMPTS - 1) as usize
        );
    }

    #[tokio::test]
    async fn total_wait_is_bounded() {
        let gate = Gate {
            left: AtomicU32::new(u32::MAX),
            calls: AtomicU32::new(0),
            error: None,
        };
        let clock = Virtual {
            slept: Mutex::new(Vec::new()),
        };
        let err = retry(&clock, || async {
            gate.next().map_err(|_| ApiError::Unavailable {
                retry_after_ms: 60_000,
            })
        })
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            ApiError::Unavailable {
                retry_after_ms: 60_000
            }
        ));
        let slept = clock.slept.lock().unwrap().clone();
        let total: Duration = slept.iter().copied().sum();
        assert!(total <= MAX_WAIT);
        assert!(slept.len() < MAX_ATTEMPTS as usize);
        assert!(gate.calls.load(Ordering::SeqCst) < MAX_ATTEMPTS);
    }

    #[tokio::test]
    async fn other_errors_are_not_retried() {
        let gate = Gate {
            left: AtomicU32::new(0),
            calls: AtomicU32::new(0),
            error: Some(ApiError::Unauthenticated),
        };
        let clock = Virtual {
            slept: Mutex::new(Vec::new()),
        };
        let err = retry(&clock, || async { gate.next() }).await.unwrap_err();
        assert!(matches!(err, ApiError::Unauthenticated));
        assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
        assert!(clock.slept.lock().unwrap().is_empty());
    }
}
