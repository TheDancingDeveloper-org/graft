//! Draft expiry and incremental object collection (`docs/design.md` §8.6).
//!
//! Three things live here:
//!
//! - the **dry run** ([`Collector::dry_run`]): what a sweep would collect.
//!   On a repository with protection records it reads them ([`Plan`]); on
//!   one without, it walks ancestry from the live drafts (the G2.9 report,
//!   [`dry_run_report`]). `--verify` ([`Collector::verify`]) checks the plan
//!   against the ancestry walk;
//! - the **sweep** ([`Collector::sweep`]): lapse, collect, expire uploads
//!   and claims, and delete, each step one metadata transaction with its
//!   journal entry, in batches, rate-limited, under a per-repository store
//!   lease.
//!   It refuses a repository without the `gc_protection` marker, and one
//!   whose deletion switch ([`set_deletion`]) the operator has not turned
//!   on;
//! - the **gauges** ([`Collector::gauges`]), `graft_orphans{state}` for the
//!   states only the collector can see, `pinned` among them.
//!
//! Nothing here reads the journal, walks history or opens a snapshot: every
//! scan is an index range over what is collectable now, and the cost of
//! collecting a record follows its own changes.

/// Whether a planted bug is on; `false` without the `planted-bugs` feature.
#[cfg(feature = "planted-bugs")]
macro_rules! planted {
    ($bugs:expr, $bug:ident) => {
        $bugs.is_planted($crate::planted::PlantedBug::$bug)
    };
}
#[cfg(not(feature = "planted-bugs"))]
macro_rules! planted {
    ($bugs:expr, $bug:ident) => {
        false
    };
}

mod ancestry;
mod journal;
mod lease;
mod plan;
#[cfg(feature = "planted-bugs")]
pub mod planted;
mod sweep;

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use graft_model::{ChangesetId, Clock, ObjectId, SystemClock};
use graft_store::{
    ChangesetRecord, MAX_PAGE, MetaStore, MetaTxn, ObjectStore, RecordState, RepoId, StoreError,
    StoreResult, TxnOutcome, Write,
};

pub use ancestry::dry_run_report;
pub use journal::{
    ADMIN_ACTION, COLLECTED, OBJECT_DELETED, ROWS_EXPIRED, WORKSPACE_CLOSED, WORKSPACE_LAPSING,
};
pub use lease::{SweepLease, SweepLeases};
pub use plan::{Plan, Verify};
pub use sweep::SweepReport;

/// Collection timings and limits.
#[derive(Clone, Debug)]
pub struct GcConfig {
    /// How long abandoned and superseded drafts are kept.
    pub draft_retention: Duration,
    /// How long a losing land candidate is kept.
    pub land_attempt_lease: Duration,
    /// How long an unclaimed upload is kept.
    pub upload_lease: Duration,
    /// Doomed-to-deleted grace; longer than any request deadline.
    pub delete_grace: Duration,
    /// Rows per batch: the page size of every scan.
    pub batch: u32,
    /// Most drafts the lapse step abandons in one transaction. A longer
    /// stack is closed in several, from the head down.
    pub max_stack: u32,
    /// Most transactions one sweep pass commits. A pass that reaches it
    /// stops and says so ([`SweepReport::more`]); the next pass continues
    /// from the indexes.
    pub pass_budget: u32,
    /// The pause after each batch of a sweep pass: the rate limit.
    pub pause: Duration,
    /// How long the store sweep lease lasts; a pass renews it at each
    /// pause. A sweeper that dies holding it blocks others this long.
    pub sweep_lease: Duration,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            draft_retention: Duration::from_secs(7 * 24 * 3600),
            land_attempt_lease: Duration::from_secs(600),
            upload_lease: Duration::from_secs(3600),
            delete_grace: Duration::from_secs(900),
            batch: 1000,
            max_stack: 1000,
            pass_budget: 10_000,
            pause: Duration::from_millis(20),
            sweep_lease: Duration::from_secs(600),
        }
    }
}

/// What a dry run found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Changesets collected (or collectable).
    pub changesets: u64,
    /// Objects doomed (or doomable).
    pub doomed: u64,
    /// Objects deleted. Always zero in a dry run.
    pub deleted: u64,
    /// Bytes reclaimed (or reclaimable): the frames of the doomed objects.
    pub bytes: u64,
}

/// What the collector needs from a changeset's stored content and trees,
/// which only the engine can decode.
pub trait Shape: Send + Sync {
    /// The parents of a stored changeset, read from its content.
    fn parents(&self, content: &[u8]) -> StoreResult<Vec<ChangesetId>>;

    /// The objects a changeset introduced: the ones it holds rows for. The
    /// answer may be a superset, because an object is doomed only when every
    /// one of its stored holder rows is collectable. It must not omit an
    /// object the changeset holds a row for, or that object is never
    /// reported.
    fn introduced(
        &self,
        repo: RepoId,
        id: ChangesetId,
        content: &[u8],
    ) -> impl Future<Output = StoreResult<Vec<ObjectId>>> + Send;

    /// Of `objects`, the rows of the superseded changeset whose content is
    /// `from`, those its successor (content `to`) still has at the same
    /// position: the rows the successor rule moves to it (design §8.6). It
    /// reads only `from`'s changed paths in both roots.
    ///
    /// The default moves every row. That is always safe, but it holds what
    /// the successor dropped for as long as the successor lives.
    fn retained(
        &self,
        repo: RepoId,
        from: &[u8],
        to: &[u8],
        objects: &[ObjectId],
    ) -> impl Future<Output = StoreResult<Vec<ObjectId>>> + Send {
        let _ = (repo, from, to);
        let all = objects.to_vec();
        async move { Ok(all) }
    }
}

/// A pause between a sweep's batches.
pub trait Pace: Send + Sync {
    /// A future that completes after `pause`.
    fn pause(&self, pause: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
}

/// The default [`Pace`]: one thread per pause, which wakes the task. The
/// server can install its runtime's timer instead.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadPace;

impl Pace for ThreadPace {
    fn pause(&self, pause: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        Box::pin(ThreadSleep::new(pause))
    }
}

/// A [`Pace`] that never waits (tests, and one-off admin passes).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoPace;

impl Pace for NoPace {
    fn pause(&self, _pause: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        Box::pin(std::future::ready(()))
    }
}

/// A sleep on its own thread, which wakes the polling task when it ends.
struct ThreadSleep {
    pause: Duration,
    done: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl ThreadSleep {
    fn new(pause: Duration) -> Self {
        Self { pause, done: None }
    }
}

impl Future for ThreadSleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        use std::sync::atomic::{AtomicBool, Ordering};
        match &self.done {
            Some(done) if done.load(Ordering::SeqCst) => std::task::Poll::Ready(()),
            Some(_) => std::task::Poll::Pending,
            None => {
                let done = Arc::new(AtomicBool::new(false));
                let (flag, waker, pause) = (done.clone(), cx.waker().clone(), self.pause);
                std::thread::spawn(move || {
                    std::thread::sleep(pause);
                    flag.store(true, Ordering::SeqCst);
                    waker.wake();
                });
                self.done = Some(done);
                std::task::Poll::Pending
            }
        }
    }
}

/// Why the collector refused or failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GcError {
    /// The store failed.
    Store(StoreError),
    /// The repository has no `gc_protection` marker: its records may lack
    /// pins or rows, so a sweep could delete what is held. Run the
    /// protection backfill first.
    Unprotected,
    /// The operator has not turned deletion on for this repository.
    DeletionOff,
    /// Another sweep holds the repository's lease.
    Busy,
    /// `--verify` found the protection plan would collect what the
    /// ancestry walk protects. Nothing was swept.
    Mismatch {
        /// Changesets the plan collects that a kept changeset descends from.
        changesets: Vec<ChangesetId>,
        /// Objects the plan dooms that a kept changeset introduced.
        objects: Vec<ObjectId>,
    },
}

impl fmt::Display for GcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GcError::Store(e) => write!(f, "store: {e:?}"),
            GcError::Unprotected => f.write_str(
                "the repository has no gc_protection marker; run the protection backfill",
            ),
            GcError::DeletionOff => f.write_str("deletion is off for this repository"),
            GcError::Busy => f.write_str("another sweep holds this repository's lease"),
            GcError::Mismatch {
                changesets,
                objects,
            } => write!(
                f,
                "verify found {} changesets and {} objects the ancestry walk protects",
                changesets.len(),
                objects.len()
            ),
        }
    }
}

impl std::error::Error for GcError {}

impl From<StoreError> for GcError {
    fn from(e: StoreError) -> Self {
        GcError::Store(e)
    }
}

/// The `graft_orphans{state}` gauges only the collector can see. Each costs
/// the population it counts, never the history.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Gauges {
    /// `pinned`: records past their expiry that pins keep. A count that
    /// grows while nothing else does is a pin leak (`fsck` repairs it).
    pub pinned: u64,
    /// `collectable`: records past their expiry with no pins.
    pub collectable: u64,
    /// `doomed`: objects no row holds, waiting out the grace period.
    pub doomed: u64,
    /// `deleting`: objects whose frame a sweep is deleting.
    pub deleting: u64,
}

impl Gauges {
    /// `(metric, state, value)` samples, for the telemetry registry.
    pub fn samples(&self) -> [(&'static str, &'static str, u64); 4] {
        [
            ("graft_orphans", "pinned", self.pinned),
            ("graft_orphans", "collectable", self.collectable),
            ("graft_orphans", "doomed", self.doomed),
            ("graft_orphans", "deleting", self.deleting),
        ]
    }
}

/// The derived-value kind of the deletion switch.
const DELETION_KIND: &str = "gc-deletion";

/// Whether the operator has turned deletion on for `repo`. Off until they
/// do.
pub async fn deletion_enabled<M: MetaStore>(meta: &M, repo: RepoId) -> StoreResult<bool> {
    Ok(meta
        .derived(repo, DELETION_KIND, 1, b"")
        .await?
        .is_some_and(|v| v == [1]))
}

/// Turns deletion on or off for `repo`, journalled as an `AdminAction` by
/// `actor` (a subject, never a token).
pub async fn set_deletion<M: MetaStore>(
    meta: &M,
    repo: RepoId,
    on: bool,
    actor: &str,
) -> StoreResult<()> {
    let txn = MetaTxn {
        preconditions: Vec::new(),
        writes: vec![
            Write::PutDerived {
                kind: DELETION_KIND.to_owned(),
                version: 1,
                key: Vec::new(),
                value: vec![u8::from(on)],
            },
            journal::deletion_switched(on, actor),
        ],
    };
    match meta.commit(repo, txn).await? {
        TxnOutcome::Committed(_) => Ok(()),
        TxnOutcome::PreconditionFailed(_) => Err(StoreError::Backend(
            "an unconditional write failed a precondition".to_owned(),
        )),
    }
}

/// The collector for the repositories of one store.
pub struct Collector<M, O> {
    meta: M,
    objects: O,
    config: GcConfig,
    clock: Arc<dyn Clock>,
    pace: Arc<dyn Pace>,
    leases: SweepLeases,
    #[cfg(feature = "planted-bugs")]
    planted: planted::PlantedBugs,
}

impl<M, O> fmt::Debug for Collector<M, O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Collector")
            .field("config", &self.config)
            .field("leases", &self.leases)
            .finish_non_exhaustive()
    }
}

impl<M: MetaStore, O: ObjectStore> Collector<M, O> {
    /// Creates a collector on the system clock, pausing on threads, with
    /// its own leases.
    pub fn new(meta: M, objects: O, config: GcConfig) -> Self {
        Self {
            meta,
            objects,
            config,
            clock: Arc::new(SystemClock),
            pace: Arc::new(ThreadPace),
            leases: SweepLeases::new(),
            #[cfg(feature = "planted-bugs")]
            planted: planted::PlantedBugs::default(),
        }
    }

    /// The same collector reading `clock`.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The same collector pausing with `pace`.
    pub fn with_pace(mut self, pace: Arc<dyn Pace>) -> Self {
        self.pace = pace;
        self
    }

    /// The same collector sharing `leases` (with the server's other
    /// collectors).
    pub fn with_leases(mut self, leases: SweepLeases) -> Self {
        self.leases = leases;
        self
    }

    /// The same collector sharing the planted bugs `bugs` (test only).
    #[cfg(feature = "planted-bugs")]
    pub fn with_planted_bugs(mut self, bugs: planted::PlantedBugs) -> Self {
        self.planted = bugs;
        self
    }

    /// The configuration.
    pub fn config(&self) -> &GcConfig {
        &self.config
    }

    /// The stores.
    pub fn stores(&self) -> (&M, &O) {
        (&self.meta, &self.objects)
    }

    /// The leases.
    pub fn leases(&self) -> &SweepLeases {
        &self.leases
    }

    fn now(&self) -> i64 {
        self.clock.now().ms
    }

    /// The time the sweep compares lease and expiry ends against.
    fn lease_now(&self) -> i64 {
        let now = self.now();
        if planted!(self.planted, LeaseBoundary) {
            return now.saturating_add(1);
        }
        now
    }

    /// What a sweep at `now_ms` would collect, doom and reclaim. It reads
    /// the protection records on a repository that has them, and walks
    /// ancestry (the G2.9 report) on one that does not.
    pub async fn dry_run<S: Shape>(
        &self,
        repo: RepoId,
        now_ms: i64,
        shape: &S,
    ) -> Result<Report, GcError> {
        if self.meta.gc_protection(repo).await? {
            Ok(self.plan(repo, now_ms, shape).await?.report())
        } else {
            Ok(
                dry_run_report(&self.meta, &self.objects, &self.config, repo, now_ms, shape)
                    .await?,
            )
        }
    }

    /// The gauges at `now_ms`.
    pub async fn gauges(&self, repo: RepoId, now_ms: i64) -> Result<Gauges, GcError> {
        let batch = self.batch();
        let mut gauges = Gauges::default();
        for state in [
            RecordState::Draft,
            RecordState::Superseded,
            RecordState::Abandoned,
        ] {
            let mut after = None;
            loop {
                let page = self.meta.list_records(repo, state, after, batch).await?;
                let Some((last, _)) = page.last() else {
                    break;
                };
                after = Some(*last);
                for (id, _) in page {
                    let Some(p) = self.meta.protection(repo, id).await? else {
                        continue;
                    };
                    if p.expires_at_ms.is_some_and(|e| e <= now_ms) {
                        if p.pins > 0 {
                            gauges.pinned += 1;
                        } else {
                            gauges.collectable += 1;
                        }
                    }
                }
            }
        }
        let mut after = None;
        loop {
            let page = self.meta.old_doomed(repo, i64::MAX, after, batch).await?;
            let Some((last, mark)) = page.last() else {
                break;
            };
            after = Some((mark.since_ms, *last));
            for (_, mark) in page {
                if mark.deleting {
                    gauges.deleting += 1;
                } else {
                    gauges.doomed += 1;
                }
            }
        }
        Ok(gauges)
    }

    /// The page size of every scan.
    fn batch(&self) -> u32 {
        self.config.batch.clamp(1, MAX_PAGE)
    }
}

/// A duration in ms, saturating.
pub(crate) fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// Whether `record` is a draft held by workspace `name`.
pub(crate) fn held_by(record: &ChangesetRecord, name: &str) -> bool {
    record.state == RecordState::Draft
        && record.holder.as_deref().and_then(|h| h.strip_prefix("ws:")) == Some(name)
}

/// The distinct parents in `parents`, in order, without `id` itself.
pub(crate) fn distinct(id: ChangesetId, parents: Vec<ChangesetId>) -> Vec<ChangesetId> {
    let mut out: Vec<ChangesetId> = Vec::with_capacity(parents.len());
    for p in parents {
        if p != id && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Every object `holder` holds a row for, paged by the holder index.
pub(crate) async fn held_objects<M: MetaStore>(
    meta: &M,
    repo: RepoId,
    holder: &str,
    batch: u32,
) -> StoreResult<Vec<ObjectId>> {
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = meta.held_objects(repo, holder, after, batch).await?;
        let Some(last) = page.last() else {
            return Ok(out);
        };
        after = Some(*last);
        out.extend(page);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_timings_match_the_design() {
        let c = GcConfig::default();
        assert_eq!(c.draft_retention, Duration::from_secs(7 * 24 * 3600));
        assert_eq!(c.land_attempt_lease, Duration::from_secs(600));
        assert_eq!(c.upload_lease, Duration::from_secs(3600));
        assert_eq!(c.delete_grace, Duration::from_secs(900));
    }

    #[test]
    fn distinct_keeps_order_and_drops_self() {
        let id = |b| ChangesetId(ObjectId([b; 32]));
        assert_eq!(
            distinct(id(1), vec![id(2), id(1), id(3), id(2)]),
            vec![id(2), id(3)]
        );
    }

    #[test]
    fn the_gauges_are_orphan_states() {
        let g = Gauges {
            pinned: 1,
            collectable: 2,
            doomed: 3,
            deleting: 4,
        };
        assert_eq!(
            g.samples().map(|(m, s, v)| (m, s, v)),
            [
                ("graft_orphans", "pinned", 1),
                ("graft_orphans", "collectable", 2),
                ("graft_orphans", "doomed", 3),
                ("graft_orphans", "deleting", 4),
            ]
        );
    }
}
