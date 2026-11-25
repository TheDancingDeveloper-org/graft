//! A metadata store that counts the calls made to the one it wraps, so a
//! test can assert a cost that must not follow history depth.

use std::sync::atomic::{AtomicU64, Ordering};

use graft_model::{ChangesetId, ObjectId, RepoPath};
use graft_store::{
    ChangesetRecord, DoomMark, Lsn, MetaStore, MetaTxn, Protection, RecordState, RefRecord, RepoId,
    StoreResult, TimedRow, TxnOutcome, WorkspaceRecord,
};

/// Counts every call, whatever it reads.
#[derive(Debug, Default)]
pub struct Counting<M> {
    pub inner: M,
    pub calls: AtomicU64,
    /// `commit_run` calls only (also counted in `calls`).
    pub runs: AtomicU64,
}

impl<M: MetaStore> MetaStore for Counting<M> {
    async fn create_repo(&self, name: &str) -> StoreResult<RepoId> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.create_repo(name).await
    }
    async fn repo(&self, name: &str) -> StoreResult<Option<RepoId>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.repo(name).await
    }
    async fn commit(&self, repo: RepoId, txn: MetaTxn) -> StoreResult<TxnOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.commit(repo, txn).await
    }
    async fn commit_run(&self, repo: RepoId, txns: Vec<MetaTxn>) -> StoreResult<Vec<TxnOutcome>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.inner.commit_run(repo, txns).await
    }
    async fn changeset(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> StoreResult<Option<(Vec<u8>, ChangesetRecord)>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.changeset(repo, id).await
    }
    async fn get_ref(&self, repo: RepoId, name: &str) -> StoreResult<Option<RefRecord>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.get_ref(repo, name).await
    }
    async fn list_refs(
        &self,
        repo: RepoId,
        prefix: &str,
        limit: u32,
    ) -> StoreResult<Vec<RefRecord>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.list_refs(repo, prefix, limit).await
    }
    async fn workspace(&self, repo: RepoId, name: &str) -> StoreResult<Option<WorkspaceRecord>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.workspace(repo, name).await
    }
    async fn expired_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.expired_workspaces(repo, before_ms, limit).await
    }
    async fn list_workspaces(
        &self,
        repo: RepoId,
        owner: &str,
        after: &str,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.list_workspaces(repo, owner, after, limit).await
    }
    async fn count_records(&self, repo: RepoId, state: RecordState) -> StoreResult<u64> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.count_records(repo, state).await
    }
    async fn count_workspaces(&self, repo: RepoId, before_ms: Option<i64>) -> StoreResult<u64> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.count_workspaces(repo, before_ms).await
    }
    async fn derived(
        &self,
        repo: RepoId,
        kind: &str,
        version: u32,
        key: &[u8],
    ) -> StoreResult<Option<Vec<u8>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.derived(repo, kind, version, key).await
    }
    async fn line_pos(
        &self,
        repo: RepoId,
        line: &str,
        id: ChangesetId,
    ) -> StoreResult<Option<u64>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.line_pos(repo, line, id).await
    }
    async fn line_holders(
        &self,
        repo: RepoId,
        id: ChangesetId,
        limit: u32,
    ) -> StoreResult<Vec<(String, u64)>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.line_holders(repo, id, limit).await
    }
    async fn scan_line_path(
        &self,
        repo: RepoId,
        line: &str,
        path: &RepoPath,
        before: u64,
        limit: u32,
    ) -> StoreResult<Vec<(u64, ChangesetId)>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner
            .scan_line_path(repo, line, path, before, limit)
            .await
    }
    async fn holders(&self, repo: RepoId, object: ObjectId) -> StoreResult<u32> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.holders(repo, object).await
    }
    async fn land(
        &self,
        repo: RepoId,
        draft: ChangesetId,
        line: &str,
    ) -> StoreResult<Option<ChangesetId>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.land(repo, draft, line).await
    }
    async fn journal(
        &self,
        repo: RepoId,
        from: Lsn,
        limit: u32,
    ) -> StoreResult<Vec<(Lsn, String, Vec<u8>)>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.journal(repo, from, limit).await
    }
    async fn list_records(
        &self,
        repo: RepoId,
        state: RecordState,
        after: Option<ChangesetId>,
        limit: u32,
    ) -> StoreResult<Vec<(ChangesetId, ChangesetRecord)>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.list_records(repo, state, after, limit).await
    }
    async fn list_all_workspaces(
        &self,
        repo: RepoId,
        after: &str,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.list_all_workspaces(repo, after, limit).await
    }
    async fn holder_rows(
        &self,
        repo: RepoId,
        object: ObjectId,
        after: Option<String>,
        limit: u32,
    ) -> StoreResult<Vec<String>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.holder_rows(repo, object, after, limit).await
    }
    async fn protection(&self, repo: RepoId, id: ChangesetId) -> StoreResult<Option<Protection>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.protection(repo, id).await
    }
    async fn expired_records(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<(i64, ChangesetId)>,
        limit: u32,
    ) -> StoreResult<Vec<(i64, ChangesetId)>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.expired_records(repo, now_ms, after, limit).await
    }
    async fn held_objects(
        &self,
        repo: RepoId,
        holder: &str,
        after: Option<ObjectId>,
        limit: u32,
    ) -> StoreResult<Vec<ObjectId>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.held_objects(repo, holder, after, limit).await
    }
    async fn expired_rows(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<TimedRow>,
        limit: u32,
    ) -> StoreResult<Vec<TimedRow>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.expired_rows(repo, now_ms, after, limit).await
    }
    async fn doom_marks(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
    ) -> StoreResult<Vec<(ObjectId, DoomMark)>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.doom_marks(repo, ids).await
    }
    async fn old_doomed(
        &self,
        repo: RepoId,
        until_ms: i64,
        after: Option<(i64, ObjectId)>,
        limit: u32,
    ) -> StoreResult<Vec<(ObjectId, DoomMark)>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.old_doomed(repo, until_ms, after, limit).await
    }
    async fn lapsed_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        after: Option<(i64, String)>,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner
            .lapsed_workspaces(repo, before_ms, after, limit)
            .await
    }
    async fn gc_protection(&self, repo: RepoId) -> StoreResult<bool> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.gc_protection(repo).await
    }
    async fn journal_tip(&self, repo: RepoId) -> StoreResult<Option<Lsn>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.journal_tip(repo).await
    }
}
