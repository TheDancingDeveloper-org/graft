//! Shared fixtures for the sweep tests: the real engine and a collector
//! over one pair of stores, each through its own probe (which counts
//! reads, can yield at every call so two tasks interleave, and can crash
//! at the n-th call); a shape that decodes real changesets and trees; an
//! executor that interleaves two futures by a schedule; and the
//! brute-force check of design §8.6.

#![allow(dead_code, unreachable_pub)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use graft_engine::{Engine, EngineConfig, LineConfig, Sleeper};
use graft_gc::{Collector, GcConfig, NoPace, Shape, SweepReport};
use graft_model::{
    Change, Changeset, ChangesetId, Clock, EntryKind, ObjectId, ObjectKind, RepoPath, Tree,
    TreeEntry, TreeId,
};
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{AdminApi, CallContext, Frame, ObjectApi, Principal, PrincipalKind, RepoApi};
use graft_store::memory::MemoryStore;
use graft_store::{
    ChangesetRecord, DoomMark, Lsn, MetaStore, MetaTxn, ObjectStore, Protection, RecordState,
    RefRecord, RepoId, StoreError, StoreResult, StoreStats, TimedRow, TxnOutcome, WorkspaceRecord,
    Write,
};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};
use graft_testkit::clock::FakeClock;
pub use graft_testkit::crash::{TempDir, block_on};

pub const LINE: &str = "trunk";
pub const MINUTE: i64 = 60 * 1000;
pub const HOUR: i64 = 60 * MINUTE;
pub const DAY: i64 = 24 * HOUR;
/// Longer than the draft retention.
pub const RETENTION_PLUS: i64 = 8 * DAY;
/// Longer than the delete grace.
pub const GRACE_PLUS: i64 = 16 * MINUTE;

// ---------------------------------------------------------------------------
// Yielding, counting, crashing.

/// A future that is pending once, and wakes itself.
pub struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

pub fn yield_now() -> YieldNow {
    YieldNow(false)
}

/// What a probe does on each call.
#[derive(Debug)]
pub struct Ctl {
    /// Yield before each call, so another task can run between two calls.
    pub yields: AtomicBool,
    /// Metadata reads (every call but `commit`).
    pub reads: AtomicU64,
    /// Object reads (`get`, `has`).
    pub object_reads: AtomicU64,
    /// Calls so far, metadata and object, for `crash_at`.
    pub calls: AtomicU64,
    /// The call that crashes: it and every later call fail.
    pub crash_at: AtomicU64,
    /// Fail every commit that sets a ref (a land server dying before its
    /// CAS).
    pub fail_ref_moves: AtomicBool,
}

impl Default for Ctl {
    fn default() -> Self {
        Self {
            yields: AtomicBool::new(false),
            reads: AtomicU64::new(0),
            object_reads: AtomicU64::new(0),
            calls: AtomicU64::new(0),
            crash_at: AtomicU64::new(u64::MAX),
            fail_ref_moves: AtomicBool::new(false),
        }
    }
}

pub fn crashed() -> StoreError {
    StoreError::Backend("crashed".to_owned())
}

impl Ctl {
    async fn gate(&self) -> StoreResult<()> {
        if self.yields.load(Ordering::SeqCst) {
            yield_now().await;
        }
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n >= self.crash_at.load(Ordering::SeqCst) {
            return Err(crashed());
        }
        Ok(())
    }

    async fn read(&self) -> StoreResult<()> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.gate().await
    }

    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::SeqCst)
    }

    pub fn object_reads(&self) -> u64 {
        self.object_reads.load(Ordering::SeqCst)
    }

    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn reset_counts(&self) {
        self.reads.store(0, Ordering::SeqCst);
        self.object_reads.store(0, Ordering::SeqCst);
        self.calls.store(0, Ordering::SeqCst);
    }

    pub fn crash_at(&self, n: u64) {
        self.calls.store(0, Ordering::SeqCst);
        self.crash_at.store(n, Ordering::SeqCst);
    }

    pub fn no_crash(&self) {
        self.crash_at.store(u64::MAX, Ordering::SeqCst);
    }
}

/// A metadata store seen through a [`Ctl`].
pub struct Probe<M> {
    pub inner: M,
    pub ctl: Arc<Ctl>,
}

impl<M: MetaStore> MetaStore for Probe<M> {
    async fn create_repo(&self, name: &str) -> StoreResult<RepoId> {
        self.ctl.gate().await?;
        self.inner.create_repo(name).await
    }
    async fn repo(&self, name: &str) -> StoreResult<Option<RepoId>> {
        self.ctl.read().await?;
        self.inner.repo(name).await
    }
    async fn commit(&self, repo: RepoId, txn: MetaTxn) -> StoreResult<TxnOutcome> {
        self.ctl.gate().await?;
        if self.ctl.fail_ref_moves.load(Ordering::SeqCst)
            && txn.writes.iter().any(|w| matches!(w, Write::SetRef(_)))
        {
            return Err(crashed());
        }
        self.inner.commit(repo, txn).await
    }
    async fn changeset(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> StoreResult<Option<(Vec<u8>, ChangesetRecord)>> {
        self.ctl.read().await?;
        self.inner.changeset(repo, id).await
    }
    async fn get_ref(&self, repo: RepoId, name: &str) -> StoreResult<Option<RefRecord>> {
        self.ctl.read().await?;
        self.inner.get_ref(repo, name).await
    }
    async fn list_refs(
        &self,
        repo: RepoId,
        prefix: &str,
        limit: u32,
    ) -> StoreResult<Vec<RefRecord>> {
        self.ctl.read().await?;
        self.inner.list_refs(repo, prefix, limit).await
    }
    async fn workspace(&self, repo: RepoId, name: &str) -> StoreResult<Option<WorkspaceRecord>> {
        self.ctl.read().await?;
        self.inner.workspace(repo, name).await
    }
    async fn expired_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.ctl.read().await?;
        self.inner.expired_workspaces(repo, before_ms, limit).await
    }
    async fn list_workspaces(
        &self,
        repo: RepoId,
        owner: &str,
        after: &str,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.ctl.read().await?;
        self.inner.list_workspaces(repo, owner, after, limit).await
    }
    async fn count_records(&self, repo: RepoId, state: RecordState) -> StoreResult<u64> {
        self.ctl.read().await?;
        self.inner.count_records(repo, state).await
    }
    async fn count_workspaces(&self, repo: RepoId, before_ms: Option<i64>) -> StoreResult<u64> {
        self.ctl.read().await?;
        self.inner.count_workspaces(repo, before_ms).await
    }
    async fn derived(
        &self,
        repo: RepoId,
        kind: &str,
        version: u32,
        key: &[u8],
    ) -> StoreResult<Option<Vec<u8>>> {
        self.ctl.read().await?;
        self.inner.derived(repo, kind, version, key).await
    }
    async fn line_pos(
        &self,
        repo: RepoId,
        line: &str,
        id: ChangesetId,
    ) -> StoreResult<Option<u64>> {
        self.ctl.read().await?;
        self.inner.line_pos(repo, line, id).await
    }
    async fn scan_line_path(
        &self,
        repo: RepoId,
        line: &str,
        path: &RepoPath,
        before: u64,
        limit: u32,
    ) -> StoreResult<Vec<(u64, ChangesetId)>> {
        self.ctl.read().await?;
        self.inner
            .scan_line_path(repo, line, path, before, limit)
            .await
    }
    async fn holders(&self, repo: RepoId, object: ObjectId) -> StoreResult<u32> {
        self.ctl.read().await?;
        self.inner.holders(repo, object).await
    }
    async fn land(
        &self,
        repo: RepoId,
        draft: ChangesetId,
        line: &str,
    ) -> StoreResult<Option<ChangesetId>> {
        self.ctl.read().await?;
        self.inner.land(repo, draft, line).await
    }
    async fn journal(
        &self,
        repo: RepoId,
        from: Lsn,
        limit: u32,
    ) -> StoreResult<Vec<(Lsn, String, Vec<u8>)>> {
        self.ctl.read().await?;
        self.inner.journal(repo, from, limit).await
    }
    async fn list_records(
        &self,
        repo: RepoId,
        state: RecordState,
        after: Option<ChangesetId>,
        limit: u32,
    ) -> StoreResult<Vec<(ChangesetId, ChangesetRecord)>> {
        self.ctl.read().await?;
        self.inner.list_records(repo, state, after, limit).await
    }
    async fn list_all_workspaces(
        &self,
        repo: RepoId,
        after: &str,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.ctl.read().await?;
        self.inner.list_all_workspaces(repo, after, limit).await
    }
    async fn holder_rows(
        &self,
        repo: RepoId,
        object: ObjectId,
        after: Option<String>,
        limit: u32,
    ) -> StoreResult<Vec<String>> {
        self.ctl.read().await?;
        self.inner.holder_rows(repo, object, after, limit).await
    }
    async fn protection(&self, repo: RepoId, id: ChangesetId) -> StoreResult<Option<Protection>> {
        self.ctl.read().await?;
        self.inner.protection(repo, id).await
    }
    async fn expired_records(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<(i64, ChangesetId)>,
        limit: u32,
    ) -> StoreResult<Vec<(i64, ChangesetId)>> {
        self.ctl.read().await?;
        self.inner.expired_records(repo, now_ms, after, limit).await
    }
    async fn held_objects(
        &self,
        repo: RepoId,
        holder: &str,
        after: Option<ObjectId>,
        limit: u32,
    ) -> StoreResult<Vec<ObjectId>> {
        self.ctl.read().await?;
        self.inner.held_objects(repo, holder, after, limit).await
    }
    async fn expired_rows(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<TimedRow>,
        limit: u32,
    ) -> StoreResult<Vec<TimedRow>> {
        self.ctl.read().await?;
        self.inner.expired_rows(repo, now_ms, after, limit).await
    }
    async fn doom_marks(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
    ) -> StoreResult<Vec<(ObjectId, DoomMark)>> {
        self.ctl.read().await?;
        self.inner.doom_marks(repo, ids).await
    }
    async fn old_doomed(
        &self,
        repo: RepoId,
        until_ms: i64,
        after: Option<(i64, ObjectId)>,
        limit: u32,
    ) -> StoreResult<Vec<(ObjectId, DoomMark)>> {
        self.ctl.read().await?;
        self.inner.old_doomed(repo, until_ms, after, limit).await
    }
    async fn lapsed_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        after: Option<(i64, String)>,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.ctl.read().await?;
        self.inner
            .lapsed_workspaces(repo, before_ms, after, limit)
            .await
    }
    async fn gc_protection(&self, repo: RepoId) -> StoreResult<bool> {
        self.ctl.read().await?;
        self.inner.gc_protection(repo).await
    }
}

/// Frames by repository and id: a standalone object store, which pairs
/// with any metadata store.
#[derive(Clone, Debug, Default)]
pub struct Frames(pub Arc<Mutex<FrameMap>>);

/// Frames by repository and id.
pub type FrameMap = BTreeMap<(RepoId, ObjectId), Vec<u8>>;

impl Frames {
    pub fn map(&self) -> std::sync::MutexGuard<'_, FrameMap> {
        self.0.lock().expect("frames")
    }

    /// The stored frames of `repo`, with their sizes.
    pub fn sizes(&self, repo: RepoId) -> BTreeMap<ObjectId, u64> {
        self.map()
            .iter()
            .filter(|((r, _), _)| *r == repo)
            .map(|((_, id), f)| (*id, f.len() as u64))
            .collect()
    }
}

impl ObjectStore for Frames {
    async fn put(&self, repo: RepoId, id: ObjectId, frame: Vec<u8>) -> StoreResult<()> {
        self.map().entry((repo, id)).or_insert(frame);
        Ok(())
    }
    async fn get(&self, repo: RepoId, id: ObjectId) -> StoreResult<Option<Vec<u8>>> {
        Ok(self.map().get(&(repo, id)).cloned())
    }
    async fn has(&self, repo: RepoId, ids: &[ObjectId]) -> StoreResult<Vec<ObjectId>> {
        let map = self.map();
        Ok(ids
            .iter()
            .copied()
            .filter(|id| map.contains_key(&(repo, *id)))
            .collect())
    }
    async fn delete(&self, repo: RepoId, id: ObjectId) -> StoreResult<()> {
        self.map().remove(&(repo, id));
        Ok(())
    }
    async fn stats(&self) -> StoreResult<StoreStats> {
        let map = self.map();
        Ok(StoreStats {
            objects: map.len() as u64,
            bytes: map.values().map(|f| f.len() as u64).sum(),
            free_bytes: u64::MAX,
        })
    }
}

/// An object store seen through a [`Ctl`].
pub struct ProbeObjects<O> {
    pub inner: O,
    pub ctl: Arc<Ctl>,
}

impl<O: ObjectStore> ObjectStore for ProbeObjects<O> {
    async fn put(&self, repo: RepoId, id: ObjectId, frame: Vec<u8>) -> StoreResult<()> {
        self.ctl.gate().await?;
        self.inner.put(repo, id, frame).await
    }
    async fn get(&self, repo: RepoId, id: ObjectId) -> StoreResult<Option<Vec<u8>>> {
        self.ctl.object_reads.fetch_add(1, Ordering::SeqCst);
        self.ctl.gate().await?;
        self.inner.get(repo, id).await
    }
    async fn has(&self, repo: RepoId, ids: &[ObjectId]) -> StoreResult<Vec<ObjectId>> {
        self.ctl.object_reads.fetch_add(1, Ordering::SeqCst);
        self.ctl.gate().await?;
        self.inner.has(repo, ids).await
    }
    async fn delete(&self, repo: RepoId, id: ObjectId) -> StoreResult<()> {
        self.ctl.gate().await?;
        self.inner.delete(repo, id).await
    }
    async fn stats(&self) -> StoreResult<StoreStats> {
        self.inner.stats().await
    }
}

/// A sleeper that only yields, so an engine waiting on a delete lets the
/// interleaved sweep run.
pub struct YieldSleeper;

impl Sleeper for YieldSleeper {
    fn sleep(&self, _ms: u64) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        Box::pin(yield_now())
    }
}

// ---------------------------------------------------------------------------
// Interleaving.

/// Runs `a` and `b` together, polling the one `schedule` names at each
/// step (`true` for `a`, cycling through the schedule), until both finish.
/// The stores yield at every call, so a schedule is an interleaving of the
/// two tasks' store calls.
pub fn interleave<A: Future, B: Future>(a: A, b: B, schedule: &[bool]) -> (A::Output, B::Output) {
    let mut a = std::pin::pin!(a);
    let mut b = std::pin::pin!(b);
    let mut cx = Context::from_waker(Waker::noop());
    let (mut out_a, mut out_b) = (None, None);
    let mut step = 0usize;
    while out_a.is_none() || out_b.is_none() {
        let pick_a = if schedule.is_empty() {
            step.is_multiple_of(2)
        } else {
            schedule[step % schedule.len()]
        };
        step += 1;
        if (pick_a && out_a.is_none()) || out_b.is_some() {
            if let Poll::Ready(v) = a.as_mut().poll(&mut cx) {
                out_a = Some(v);
            }
        } else if let Poll::Ready(v) = b.as_mut().poll(&mut cx) {
            out_b = Some(v);
        }
    }
    (out_a.expect("a"), out_b.expect("b"))
}

// ---------------------------------------------------------------------------
// The shape of real changesets.

/// Reads real changesets and trees for the collector, as the engine's own
/// shape does, plus the successor rule.
pub struct RealShape<O> {
    pub objects: O,
}

fn decode(content: &[u8]) -> StoreResult<Changeset> {
    Changeset::decode(content).map_err(|_| StoreError::Corrupt("changeset".to_owned()))
}

pub async fn read_tree<O: ObjectStore>(
    objects: &O,
    repo: RepoId,
    id: ObjectId,
) -> StoreResult<Tree> {
    let bytes = objects
        .get(repo, id)
        .await?
        .ok_or_else(|| StoreError::Corrupt(format!("tree {} is missing", id.to_hex())))?;
    let payload = Frame::from_bytes(&bytes)
        .map_err(|e| StoreError::Corrupt(format!("{e:?}")))?
        .decode_verified(id)
        .map_err(|e| StoreError::Corrupt(format!("{e:?}")))?;
    Tree::decode(&payload).map_err(|e| StoreError::Corrupt(format!("{e:?}")))
}

/// `(position, id)` along `path` in the snapshot at `root`: the root, each
/// directory, and the entry itself.
async fn along<O: ObjectStore>(
    objects: &O,
    repo: RepoId,
    root: TreeId,
    path: &RepoPath,
) -> StoreResult<Vec<(String, ObjectId)>> {
    let mut out = vec![(String::new(), root.0)];
    let mut dir = root.0;
    let mut position = String::new();
    for name in path.components() {
        let tree = read_tree(objects, repo, dir).await?;
        let Some(entry) = tree.get(name) else {
            break;
        };
        position.push('/');
        position.push_str(name);
        out.push((position.clone(), entry.id));
        if !entry.kind.is_tree() {
            break;
        }
        dir = entry.id;
    }
    Ok(out)
}

impl<O: ObjectStore> Shape for RealShape<O> {
    fn parents(&self, content: &[u8]) -> StoreResult<Vec<ChangesetId>> {
        Ok(decode(content)?.parents)
    }

    async fn introduced(
        &self,
        repo: RepoId,
        _id: ChangesetId,
        content: &[u8],
    ) -> StoreResult<Vec<ObjectId>> {
        introduced_paths(&self.objects, repo, &decode(content)?).await
    }

    async fn retained(
        &self,
        repo: RepoId,
        from: &[u8],
        to: &[u8],
        objects: &[ObjectId],
    ) -> StoreResult<Vec<ObjectId>> {
        let (from, to) = (decode(from)?, decode(to)?);
        let mut here: HashSet<(String, ObjectId)> = HashSet::new();
        let mut there: HashSet<(String, ObjectId)> = HashSet::new();
        for change in &from.changes {
            here.extend(along(&self.objects, repo, from.root, change.path()).await?);
            there.extend(along(&self.objects, repo, to.root, change.path()).await?);
        }
        let kept: HashSet<ObjectId> = here.intersection(&there).map(|(_, id)| *id).collect();
        Ok(objects
            .iter()
            .copied()
            .filter(|o| kept.contains(o))
            .collect())
    }
}

/// What a changeset holds rows for: every object along each changed path
/// in its own snapshot (design §8.6).
async fn introduced_paths<O: ObjectStore>(
    objects: &O,
    repo: RepoId,
    cs: &Changeset,
) -> StoreResult<Vec<ObjectId>> {
    let mut out = vec![cs.root.0];
    for change in &cs.changes {
        for (_, id) in along(objects, repo, cs.root, change.path()).await? {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// The world: an engine and a collector over one pair of stores.

pub fn engine_config() -> EngineConfig {
    EngineConfig {
        lines: vec![LineConfig::direct(LINE)],
        ..EngineConfig::default()
    }
}

pub fn gc_config() -> GcConfig {
    GcConfig {
        batch: 3,
        pause: Duration::ZERO,
        ..GcConfig::default()
    }
}

pub type Meta<M> = Probe<Arc<M>>;
pub type Objs = ProbeObjects<Frames>;

pub struct World<M> {
    pub meta: Arc<M>,
    pub frames: Frames,
    pub clock: Arc<FakeClock>,
    pub engine_ctl: Arc<Ctl>,
    pub gc_ctl: Arc<Ctl>,
    pub engine: Engine<Meta<M>, Objs>,
    pub gc: Collector<Meta<M>, Objs>,
    pub shape: RealShape<Objs>,
    pub repo: RepoId,
    pub name: String,
    pub cx: CallContext,
    pub requests: AtomicU64,
    _dir: Option<TempDir>,
}

pub fn memory() -> World<MemoryStore> {
    World::new(
        Arc::new(MemoryStore::new()),
        Frames::default(),
        None,
        gc_config(),
    )
}

pub fn sqlite() -> World<SqliteMetaStore> {
    let dir = TempDir::new("graft-gc-sqlite");
    let meta = SqliteMetaStore::open(SqliteConfig::new(dir.path().join("meta.sqlite")))
        .expect("open sqlite");
    World::new(Arc::new(meta), Frames::default(), Some(dir), gc_config())
}

pub fn cx(name: &str) -> CallContext {
    CallContext {
        principal: Principal {
            subject: "tester".to_owned(),
            kind: PrincipalKind::Human,
            session: None,
            work_item: None,
            scopes: vec![format!("repo:{name}:admin")],
        },
        request_id: "req-1".to_owned(),
        deadline_ms: None,
    }
}

pub fn p(s: &str) -> RepoPath {
    RepoPath::new(s).expect("valid path")
}

pub fn blob_id(data: &[u8]) -> ObjectId {
    ObjectId::hash(ObjectKind::Blob, data)
}

pub fn put(path: &str, data: &[u8]) -> Change {
    Change::Put {
        path: p(path),
        kind: EntryKind::File,
        id: blob_id(data),
    }
}

impl<M: MetaStore + 'static> World<M> {
    pub fn new(meta: Arc<M>, frames: Frames, dir: Option<TempDir>, config: GcConfig) -> Self {
        let clock = Arc::new(FakeClock::at(1_000_000));
        let engine_ctl = Arc::new(Ctl::default());
        let gc_ctl = Arc::new(Ctl::default());
        let engine = Engine::new(
            Arc::new(Probe {
                inner: meta.clone(),
                ctl: engine_ctl.clone(),
            }),
            Arc::new(ProbeObjects {
                inner: frames.clone(),
                ctl: engine_ctl.clone(),
            }),
            clock.clone(),
            engine_config(),
        )
        .with_sleeper(Arc::new(YieldSleeper));
        let gc = Collector::new(
            Probe {
                inner: meta.clone(),
                ctl: gc_ctl.clone(),
            },
            ProbeObjects {
                inner: frames.clone(),
                ctl: gc_ctl.clone(),
            },
            config,
        )
        .with_clock(clock.clone())
        .with_pace(Arc::new(NoPace));
        let shape = RealShape {
            objects: ProbeObjects {
                inner: frames.clone(),
                ctl: gc_ctl.clone(),
            },
        };
        let name = "r".to_owned();
        let cx = cx(&name);
        block_on(engine.create_repo(&cx, name.clone())).expect("create repo");
        let repo = block_on(meta.repo(&name)).expect("repo").expect("exists");
        Self {
            meta,
            frames,
            clock,
            engine_ctl,
            gc_ctl,
            engine,
            gc,
            shape,
            repo,
            name,
            cx,
            requests: AtomicU64::new(0),
            _dir: dir,
        }
    }

    pub fn now(&self) -> i64 {
        self.clock.now().ms
    }

    pub fn advance(&self, ms: i64) {
        self.clock.advance(ms);
    }

    /// Both probes yield at every call (for [`interleave`]), or neither.
    pub fn yields(&self, on: bool) {
        self.engine_ctl.yields.store(on, Ordering::SeqCst);
        self.gc_ctl.yields.store(on, Ordering::SeqCst);
    }

    pub fn sweep(&self) -> SweepReport {
        block_on(self.gc.sweep(self.repo, &self.shape, false)).expect("sweep")
    }

    /// Sweeps until a pass changes nothing and no object is left doomed,
    /// the clock moving past the grace period between passes. Returns the
    /// summed reports.
    pub fn drain(&self) -> SweepReport {
        let mut total = SweepReport::default();
        for _ in 0..64 {
            let r = self.sweep();
            let idle = r.lapsed + r.collected + r.expired + r.doomed + r.deleted + r.recovered == 0
                && !r.more;
            add(&mut total, &r);
            let marks =
                block_on(self.meta.old_doomed(self.repo, i64::MAX, None, 1)).expect("marks");
            if idle && marks.is_empty() {
                return total;
            }
            self.advance(GRACE_PLUS);
        }
        panic!("the sweep never settled: {total:?}");
    }

    // -- engine shortcuts ---------------------------------------------------

    pub fn upload(&self, blobs: &[&[u8]]) -> ApiResult<Vec<ObjectId>> {
        block_on(self.upload_async(blobs))
    }

    pub async fn upload_async(&self, blobs: &[&[u8]]) -> ApiResult<Vec<ObjectId>> {
        let frames = blobs
            .iter()
            .map(|b| Frame::encode(ObjectKind::Blob, b))
            .collect();
        self.engine
            .put_objects(
                &self.cx,
                PutObjects {
                    repo: self.name.clone(),
                    blobs: frames,
                },
            )
            .await
    }

    pub fn request(
        &self,
        parents: Vec<ChangesetId>,
        changes: Vec<Change>,
        workspace: Option<&str>,
    ) -> CreateChangeset {
        CreateChangeset {
            repo: self.name.clone(),
            parents,
            changes,
            author: graft_model::Author {
                name: "Tess".to_owned(),
                email: "tess@example.com".to_owned(),
                kind: graft_model::AuthorKind::Agent,
            },
            authored_at: graft_model::Timestamp {
                ms: 1_700_000_000_000,
                offset_min: 0,
            },
            // Each request is a distinct changeset, even with the same
            // parents and changes.
            message: format!("change {}", self.requests.fetch_add(1, Ordering::SeqCst)),
            metadata: graft_model::Metadata::new(),
            inline_blobs: Vec::new(),
            workspace: workspace.map(str::to_owned),
        }
    }

    pub fn create(
        &self,
        parents: Vec<ChangesetId>,
        changes: Vec<Change>,
        workspace: Option<&str>,
    ) -> ApiResult<Created> {
        block_on(
            self.engine
                .create_changeset(&self.cx, self.request(parents, changes, workspace)),
        )
    }

    /// Uploads `data` and commits it at `path` on `parents`.
    pub fn commit(
        &self,
        parents: Vec<ChangesetId>,
        path: &str,
        data: &[u8],
        workspace: Option<&str>,
    ) -> ApiResult<Created> {
        self.upload(&[data])?;
        self.create(parents, vec![put(path, data)], workspace)
    }

    pub fn update_ref(
        &self,
        expected: Option<ChangesetId>,
        new: ChangesetId,
    ) -> ApiResult<RefView> {
        block_on(self.engine.update_ref(
            &self.cx,
            UpdateRef {
                repo: self.name.clone(),
                name: LINE.to_owned(),
                expected,
                new,
                kind: RefKind::Line,
            },
        ))
    }

    pub fn tip(&self) -> ChangesetId {
        block_on(self.engine.resolve_ref(
            &self.cx,
            ResolveRef {
                repo: self.name.clone(),
                name: LINE.to_owned(),
            },
        ))
        .expect("line")
        .target
    }

    /// A line at a landed root with `a`, `b` and `d/c`.
    pub fn seed(&self) -> ChangesetId {
        self.upload(&[b"a0", b"b0", b"c0"]).expect("upload");
        let mut changes = vec![put("a", b"a0"), put("b", b"b0"), put("d/c", b"c0")];
        changes.sort_by(|x, y| x.path().cmp(y.path()));
        let root = self.create(vec![], changes, None).expect("root").id;
        self.update_ref(None, root).expect("line");
        // A root is allowed only in an empty repository, so the switch
        // (journalled) comes after it.
        block_on(graft_gc::set_deletion(
            &*self.meta,
            self.repo,
            true,
            "tester",
        ))
        .expect("switch on");
        root
    }

    pub fn wref(&self, ws: &str) -> WorkspaceRef {
        WorkspaceRef {
            repo: self.name.clone(),
            name: ws.to_owned(),
        }
    }

    pub fn open(&self, ws: &str) -> ApiResult<WorkspaceView> {
        block_on(graft_proto::WorkspaceApi::open_workspace(
            &self.engine,
            &self.cx,
            OpenWorkspace {
                repo: self.name.clone(),
                name: Some(ws.to_owned()),
                line: LINE.to_owned(),
                at: None,
                lease_ms: 0,
            },
        ))
    }

    pub fn close(&self, ws: &str) -> ApiResult<()> {
        block_on(graft_proto::WorkspaceApi::close_workspace(
            &self.engine,
            &self.cx,
            self.wref(ws),
            false,
        ))
        .map(|_| ())
    }

    pub fn land(&self, head: ChangesetId) -> ApiResult<Landed> {
        block_on(self.engine.land(
            &self.cx,
            Land {
                repo: self.name.clone(),
                head,
                line: LINE.to_owned(),
                squash: false,
                merge: MergeMode::File,
                deadline_ms: None,
            },
        ))
    }

    pub fn record(&self, id: ChangesetId) -> ChangesetRecord {
        block_on(self.meta.changeset(self.repo, id))
            .expect("read")
            .expect("stored")
            .1
    }

    pub fn protection(&self, id: ChangesetId) -> Protection {
        block_on(self.meta.protection(self.repo, id))
            .expect("read")
            .expect("stored")
    }

    pub fn marks(&self, id: ObjectId) -> Option<DoomMark> {
        block_on(self.meta.doom_marks(self.repo, &[id]))
            .expect("marks")
            .pop()
            .map(|(_, m)| m)
    }

    pub fn has_frame(&self, id: ObjectId) -> bool {
        self.frames.map().contains_key(&(self.repo, id))
    }

    pub fn holders_of(&self, id: ObjectId) -> Vec<String> {
        block_on(self.meta.holder_rows(self.repo, id, None, 1000)).expect("rows")
    }

    /// The whole snapshot of a stored changeset, read directly.
    pub fn snapshot_of(&self, id: ChangesetId) -> Result<HashSet<ObjectId>, String> {
        let (content, _) = block_on(self.meta.changeset(self.repo, id))
            .map_err(|e| format!("{e:?}"))?
            .ok_or_else(|| format!("{id} is not stored"))?;
        let cs = Changeset::decode(&content).map_err(|e| format!("{e:?}"))?;
        snapshot(&self.frames, self.repo, cs.root.0)
    }

    pub fn check(&self) -> Result<(), String> {
        check(&*self.meta, &self.frames, self.repo)
    }
}

pub fn add(total: &mut SweepReport, r: &SweepReport) {
    total.lapsed += r.lapsed;
    total.abandoned += r.abandoned;
    total.collected += r.collected;
    total.moved += r.moved;
    total.expired += r.expired;
    total.doomed += r.doomed;
    total.deleted += r.deleted;
    total.recovered += r.recovered;
    total.bytes += r.bytes;
    total.skipped += r.skipped;
    total.more = r.more;
}

// ---------------------------------------------------------------------------
// The brute-force check.

pub struct Stored {
    pub id: ChangesetId,
    pub record: ChangesetRecord,
    /// The content; `None` for a collected tombstone, which keeps none.
    pub cs: Option<Changeset>,
    pub protection: Protection,
}

impl Stored {
    /// The content of a record that is not collected.
    pub fn cs(&self) -> &Changeset {
        self.cs.as_ref().expect("a live record has content")
    }
}

pub fn all_records<M: MetaStore>(meta: &M, repo: RepoId) -> Vec<Stored> {
    let mut out = Vec::new();
    for state in [
        RecordState::Draft,
        RecordState::Landed,
        RecordState::Superseded,
        RecordState::Abandoned,
        RecordState::Collected,
    ] {
        let mut after = None;
        loop {
            let page = block_on(meta.list_records(repo, state, after, 100)).expect("list");
            let Some((last, _)) = page.last() else {
                break;
            };
            after = Some(*last);
            for (id, record) in page {
                let (content, _) = block_on(meta.changeset(repo, id))
                    .expect("read")
                    .expect("stored");
                let protection = block_on(meta.protection(repo, id))
                    .expect("protection")
                    .expect("stored");
                let cs = if record.state == RecordState::Collected {
                    assert!(content.is_empty(), "the tombstone {id} keeps content");
                    None
                } else {
                    Some(Changeset::decode(&content).expect("decode"))
                };
                out.push(Stored {
                    id,
                    record,
                    cs,
                    protection,
                });
            }
        }
    }
    out
}

pub fn rows_of<M: MetaStore>(meta: &M, repo: RepoId, holder: &str) -> HashSet<ObjectId> {
    let mut out = HashSet::new();
    let mut after = None;
    loop {
        let page = block_on(meta.held_objects(repo, holder, after, 100)).expect("held");
        let Some(last) = page.last() else {
            return out;
        };
        after = Some(*last);
        out.extend(page);
    }
}

/// Every object in the snapshot rooted at `root`, by walking all of it.
/// A missing or corrupt frame is an error.
pub fn snapshot<O: ObjectStore>(
    frames: &O,
    repo: RepoId,
    root: ObjectId,
) -> Result<HashSet<ObjectId>, String> {
    let mut out = HashSet::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if !out.insert(id) {
            continue;
        }
        let tree = block_on(read_tree(frames, repo, id)).map_err(|e| format!("{e:?}"))?;
        for e in tree.entries {
            match e.kind {
                EntryKind::Tree => stack.push(e.id),
                EntryKind::Submodule => {}
                _ => {
                    let stored =
                        block_on(frames.has(repo, &[e.id])).map_err(|e| format!("{e:?}"))?;
                    if stored.is_empty() {
                        return Err(format!("blob {} is missing", e.id.to_hex()));
                    }
                    out.insert(e.id);
                }
            }
        }
    }
    Ok(out)
}

/// Design §8.6 by brute force, after any interleaving of writes and
/// sweeps:
///
/// - every non-collected changeset's snapshot is whole (every frame is
///   stored), so nothing a live or landed changeset reaches was deleted;
/// - every object in it has a row held by a changeset on its first-parent
///   chain, each link pinned by the one above it or landed;
/// - pins on a non-landed record equal its non-collected, non-landed
///   children; landed and collected records have none;
/// - no object with a row is doomed.
pub fn check<M: MetaStore, O: ObjectStore>(
    meta: &M,
    frames: &O,
    repo: RepoId,
) -> Result<(), String> {
    let records = all_records(meta, repo);
    let by_id: HashMap<ChangesetId, &Stored> = records.iter().map(|s| (s.id, s)).collect();

    let mut children: HashMap<ChangesetId, u32> = HashMap::new();
    for s in &records {
        if matches!(s.record.state, RecordState::Collected | RecordState::Landed) {
            continue;
        }
        let mut seen = HashSet::new();
        for p in &s.cs().parents {
            if seen.insert(*p) {
                *children.entry(*p).or_default() += 1;
            }
        }
    }
    for s in &records {
        let want = match s.record.state {
            RecordState::Landed | RecordState::Collected => 0,
            _ => children.get(&s.id).copied().unwrap_or(0),
        };
        if s.protection.pins != want {
            return Err(format!(
                "{} ({:?}) has {} pins, not {want}",
                s.id, s.record.state, s.protection.pins
            ));
        }
        if s.record.state == RecordState::Collected
            && let Some(child) = children.get(&s.id).filter(|n| **n > 0)
        {
            return Err(format!("collected {} has {child} live children", s.id));
        }
    }

    let mut rows: HashMap<ChangesetId, HashSet<ObjectId>> = HashMap::new();
    for s in &records {
        rows.insert(s.id, rows_of(meta, repo, &s.id.to_string()));
    }
    for s in records
        .iter()
        .filter(|s| s.record.state != RecordState::Collected)
    {
        let mut held: HashSet<ObjectId> = HashSet::new();
        let mut at = Some(s.id);
        while let Some(id) = at {
            let link = by_id
                .get(&id)
                .ok_or_else(|| format!("{id} on the chain of {} is not stored", s.id))?;
            if link.record.state == RecordState::Collected {
                return Err(format!("{id} on the chain of {} is collected", s.id));
            }
            held.extend(rows[&id].iter().copied());
            if link.record.state != RecordState::Landed && id != s.id && link.protection.pins == 0 {
                return Err(format!("{id} on the chain of {} has no pin", s.id));
            }
            at = link.cs().first_parent();
        }
        let objects = snapshot(frames, repo, s.cs().root.0)
            .map_err(|e| format!("{} ({:?}): {e}", s.id, s.record.state))?;
        let unheld: Vec<String> = objects
            .iter()
            .filter(|o| !held.contains(o))
            .map(|o| o.to_hex())
            .collect();
        if !unheld.is_empty() {
            return Err(format!("{} reaches unheld objects {unheld:?}", s.id));
        }
        let marks = block_on(meta.doom_marks(repo, &objects.iter().copied().collect::<Vec<_>>()))
            .map_err(|e| format!("{e:?}"))?;
        if let Some((o, mark)) = marks.first() {
            return Err(format!(
                "{} reaches doomed object {} ({mark:?})",
                s.id,
                o.to_hex()
            ));
        }
    }
    Ok(())
}

/// The objects every landed changeset reaches, by brute force.
pub fn landed_reach<M: MetaStore, O: ObjectStore>(
    meta: &M,
    frames: &O,
    repo: RepoId,
) -> Result<HashSet<ObjectId>, String> {
    let mut out = HashSet::new();
    for s in all_records(meta, repo)
        .iter()
        .filter(|s| s.record.state != RecordState::Collected)
    {
        out.extend(snapshot(frames, repo, s.cs().root.0)?);
    }
    Ok(out)
}

/// A tree frame for a directory holding one file.
pub fn dir_tree(data: &[u8]) -> (ObjectId, Vec<u8>) {
    let tree = Tree {
        entries: vec![TreeEntry {
            name: "f".to_owned(),
            kind: EntryKind::File,
            id: blob_id(data),
        }],
    };
    let payload = tree.encode();
    let frame = Frame::encode(ObjectKind::Tree, &payload);
    (tree.id().0, frame.to_bytes())
}

/// Commits writes with no preconditions.
pub fn commit_raw<M: MetaStore>(meta: &M, repo: RepoId, writes: Vec<Write>) {
    let outcome = block_on(meta.commit(
        repo,
        MetaTxn {
            preconditions: Vec::new(),
            writes,
        },
    ))
    .expect("commit");
    assert!(matches!(outcome, TxnOutcome::Committed(_)), "{outcome:?}");
}
