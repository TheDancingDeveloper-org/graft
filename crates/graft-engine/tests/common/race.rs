//! Deterministic races: a metadata store that yields before every call, an
//! executor that polls a set of futures in a chosen order, and a sleeper
//! that only yields. Together they interleave operations at every store
//! call, reproducibly from a schedule.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use graft_engine::Sleeper;
use graft_model::{ChangesetId, ObjectId, RepoPath};
use graft_store::{
    ChangesetRecord, Lsn, MetaStore, MetaTxn, RecordState, RefRecord, RepoId, StoreResult,
    TxnOutcome, WorkspaceRecord,
};

/// Pending once, then ready.
#[derive(Default)]
pub struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// A sleeper that yields once instead of sleeping.
#[derive(Clone, Copy, Debug, Default)]
pub struct YieldSleeper;

impl Sleeper for YieldSleeper {
    fn sleep(&self, _ms: u64) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        Box::pin(YieldNow::default())
    }
}

/// A metadata store that yields before each call to the one it wraps.
#[derive(Debug, Default)]
pub struct Yielding<M>(pub M);

impl<M: MetaStore> MetaStore for Yielding<M> {
    async fn create_repo(&self, name: &str) -> StoreResult<RepoId> {
        YieldNow::default().await;
        self.0.create_repo(name).await
    }
    async fn repo(&self, name: &str) -> StoreResult<Option<RepoId>> {
        YieldNow::default().await;
        self.0.repo(name).await
    }
    async fn commit(&self, repo: RepoId, txn: MetaTxn) -> StoreResult<TxnOutcome> {
        YieldNow::default().await;
        self.0.commit(repo, txn).await
    }
    async fn changeset(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> StoreResult<Option<(Vec<u8>, ChangesetRecord)>> {
        YieldNow::default().await;
        self.0.changeset(repo, id).await
    }
    async fn get_ref(&self, repo: RepoId, name: &str) -> StoreResult<Option<RefRecord>> {
        YieldNow::default().await;
        self.0.get_ref(repo, name).await
    }
    async fn list_refs(
        &self,
        repo: RepoId,
        prefix: &str,
        limit: u32,
    ) -> StoreResult<Vec<RefRecord>> {
        YieldNow::default().await;
        self.0.list_refs(repo, prefix, limit).await
    }
    async fn workspace(&self, repo: RepoId, name: &str) -> StoreResult<Option<WorkspaceRecord>> {
        YieldNow::default().await;
        self.0.workspace(repo, name).await
    }
    async fn expired_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        YieldNow::default().await;
        self.0.expired_workspaces(repo, before_ms, limit).await
    }
    async fn list_workspaces(
        &self,
        repo: RepoId,
        owner: &str,
        after: &str,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        YieldNow::default().await;
        self.0.list_workspaces(repo, owner, after, limit).await
    }
    async fn count_records(&self, repo: RepoId, state: RecordState) -> StoreResult<u64> {
        YieldNow::default().await;
        self.0.count_records(repo, state).await
    }
    async fn count_workspaces(&self, repo: RepoId, before_ms: Option<i64>) -> StoreResult<u64> {
        YieldNow::default().await;
        self.0.count_workspaces(repo, before_ms).await
    }
    async fn derived(
        &self,
        repo: RepoId,
        kind: &str,
        version: u32,
        key: &[u8],
    ) -> StoreResult<Option<Vec<u8>>> {
        YieldNow::default().await;
        self.0.derived(repo, kind, version, key).await
    }
    async fn line_pos(
        &self,
        repo: RepoId,
        line: &str,
        id: ChangesetId,
    ) -> StoreResult<Option<u64>> {
        YieldNow::default().await;
        self.0.line_pos(repo, line, id).await
    }
    async fn scan_line_path(
        &self,
        repo: RepoId,
        line: &str,
        path: &RepoPath,
        before: u64,
        limit: u32,
    ) -> StoreResult<Vec<(u64, ChangesetId)>> {
        YieldNow::default().await;
        self.0.scan_line_path(repo, line, path, before, limit).await
    }
    async fn holders(&self, repo: RepoId, object: ObjectId) -> StoreResult<u32> {
        YieldNow::default().await;
        self.0.holders(repo, object).await
    }
    async fn land(
        &self,
        repo: RepoId,
        draft: ChangesetId,
        line: &str,
    ) -> StoreResult<Option<ChangesetId>> {
        YieldNow::default().await;
        self.0.land(repo, draft, line).await
    }
    async fn journal(
        &self,
        repo: RepoId,
        from: Lsn,
        limit: u32,
    ) -> StoreResult<Vec<(Lsn, String, Vec<u8>)>> {
        YieldNow::default().await;
        self.0.journal(repo, from, limit).await
    }
    async fn list_records(
        &self,
        repo: RepoId,
        state: RecordState,
        after: Option<ChangesetId>,
        limit: u32,
    ) -> StoreResult<Vec<(ChangesetId, ChangesetRecord)>> {
        YieldNow::default().await;
        self.0.list_records(repo, state, after, limit).await
    }
    async fn list_all_workspaces(
        &self,
        repo: RepoId,
        after: &str,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        YieldNow::default().await;
        self.0.list_all_workspaces(repo, after, limit).await
    }
    async fn holder_rows(
        &self,
        repo: RepoId,
        object: ObjectId,
        after: Option<String>,
        limit: u32,
    ) -> StoreResult<Vec<String>> {
        YieldNow::default().await;
        self.0.holder_rows(repo, object, after, limit).await
    }
}

/// A boxed future the scheduler drives.
pub type Task<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Polls `tasks` one step at a time until all finish. Step `i` polls the
/// live task `schedule[i % len] % live`, then calls `observe`. The tasks
/// must only ever wait on yields (no timers, no other threads), so the
/// result depends on the schedule alone.
pub fn run_scheduled<T>(
    mut tasks: Vec<Task<'_, T>>,
    schedule: &[usize],
    mut observe: impl FnMut(),
) -> Vec<T> {
    assert!(!schedule.is_empty(), "a schedule");
    let mut results: Vec<Option<T>> = (0..tasks.len()).map(|_| None).collect();
    let mut live: Vec<usize> = (0..tasks.len()).collect();
    let mut cx = Context::from_waker(Waker::noop());
    let mut step = 0usize;
    while !live.is_empty() {
        let pick = schedule[step % schedule.len()] % live.len();
        step += 1;
        assert!(step < 10_000_000, "the tasks never finished");
        let i = live[pick];
        if let Poll::Ready(out) = tasks[i].as_mut().poll(&mut cx) {
            results[i] = Some(out);
            live.remove(pick);
        }
        observe();
    }
    results
        .into_iter()
        .map(|r| r.expect("every task finished"))
        .collect()
}
