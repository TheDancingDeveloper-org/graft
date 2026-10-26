//! A `MetaStore` wrapper that counts what the review queue reads, so the
//! cost test runs on both stores.
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use graft_model::{ChangesetId, ObjectId, RepoPath};
use graft_store::*;

pub(crate) struct Counting<M> {
    pub(crate) inner: M,
    /// Review queue rows returned.
    pub(crate) rows: Arc<AtomicU64>,
    /// Every other read call.
    pub(crate) reads: Arc<AtomicU64>,
    /// Journal entries returned.
    pub(crate) entries: Arc<AtomicU64>,
}

impl<M> Counting<M> {
    pub(crate) fn new(inner: M) -> Self {
        Self {
            inner,
            rows: Arc::default(),
            reads: Arc::default(),
            entries: Arc::default(),
        }
    }

    fn tick(&self) {
        self.reads.fetch_add(1, Ordering::Relaxed);
    }

    /// Journal entries returned so far.
    pub(crate) fn journal_entries(&self) -> u64 {
        self.entries.load(Ordering::Relaxed)
    }

    /// (queue rows, other reads) so far.
    pub(crate) fn counts(&self) -> (u64, u64) {
        (
            self.rows.load(Ordering::Relaxed),
            self.reads.load(Ordering::Relaxed),
        )
    }
}

impl<M: MetaStore> MetaStore for Counting<M> {
    fn create_repo(&self, name: &str) -> impl Future<Output = StoreResult<RepoId>> + Send {
        self.inner.create_repo(name)
    }
    fn repo(&self, name: &str) -> impl Future<Output = StoreResult<Option<RepoId>>> + Send {
        self.inner.repo(name)
    }
    fn commit(
        &self,
        repo: RepoId,
        txn: MetaTxn,
    ) -> impl Future<Output = StoreResult<TxnOutcome>> + Send {
        self.inner.commit(repo, txn)
    }
    fn commit_run(
        &self,
        repo: RepoId,
        txns: Vec<MetaTxn>,
    ) -> impl Future<Output = StoreResult<Vec<TxnOutcome>>> + Send {
        self.inner.commit_run(repo, txns)
    }
    fn changeset(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> impl Future<Output = StoreResult<Option<(Vec<u8>, ChangesetRecord)>>> + Send {
        self.tick();
        self.inner.changeset(repo, id)
    }
    fn get_ref(
        &self,
        repo: RepoId,
        name: &str,
    ) -> impl Future<Output = StoreResult<Option<RefRecord>>> + Send {
        self.tick();
        self.inner.get_ref(repo, name)
    }
    fn list_refs(
        &self,
        repo: RepoId,
        prefix: &str,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<RefRecord>>> + Send {
        self.tick();
        self.inner.list_refs(repo, prefix, limit)
    }
    fn workspace(
        &self,
        repo: RepoId,
        name: &str,
    ) -> impl Future<Output = StoreResult<Option<WorkspaceRecord>>> + Send {
        self.tick();
        self.inner.workspace(repo, name)
    }
    fn expired_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send {
        self.tick();
        self.inner.expired_workspaces(repo, before_ms, limit)
    }
    fn list_workspaces(
        &self,
        repo: RepoId,
        owner: &str,
        after: &str,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send {
        self.tick();
        self.inner.list_workspaces(repo, owner, after, limit)
    }
    fn count_records(
        &self,
        repo: RepoId,
        state: RecordState,
    ) -> impl Future<Output = StoreResult<u64>> + Send {
        self.tick();
        self.inner.count_records(repo, state)
    }
    fn count_workspaces(
        &self,
        repo: RepoId,
        before_ms: Option<i64>,
    ) -> impl Future<Output = StoreResult<u64>> + Send {
        self.tick();
        self.inner.count_workspaces(repo, before_ms)
    }
    fn derived(
        &self,
        repo: RepoId,
        kind: &str,
        version: u32,
        key: &[u8],
    ) -> impl Future<Output = StoreResult<Option<Vec<u8>>>> + Send {
        self.tick();
        self.inner.derived(repo, kind, version, key)
    }
    fn line_pos(
        &self,
        repo: RepoId,
        line: &str,
        id: ChangesetId,
    ) -> impl Future<Output = StoreResult<Option<u64>>> + Send {
        self.tick();
        self.inner.line_pos(repo, line, id)
    }
    fn line_holders(
        &self,
        repo: RepoId,
        id: ChangesetId,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(String, u64)>>> + Send {
        self.tick();
        self.inner.line_holders(repo, id, limit)
    }
    fn scan_line_path(
        &self,
        repo: RepoId,
        line: &str,
        path: &RepoPath,
        before: u64,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(u64, ChangesetId)>>> + Send {
        self.tick();
        self.inner.scan_line_path(repo, line, path, before, limit)
    }
    fn line_fork(
        &self,
        repo: RepoId,
        line: &str,
    ) -> impl Future<Output = StoreResult<Option<(String, u64)>>> + Send {
        self.tick();
        self.inner.line_fork(repo, line)
    }
    fn line_merges(
        &self,
        repo: RepoId,
        dst: &str,
        src: Option<&str>,
        at_or_below: u64,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<LineMergeRow>>> + Send {
        self.tick();
        self.inner.line_merges(repo, dst, src, at_or_below, limit)
    }
    fn holders(
        &self,
        repo: RepoId,
        object: ObjectId,
    ) -> impl Future<Output = StoreResult<u32>> + Send {
        self.tick();
        self.inner.holders(repo, object)
    }
    fn land(
        &self,
        repo: RepoId,
        draft: ChangesetId,
        line: &str,
    ) -> impl Future<Output = StoreResult<Option<ChangesetId>>> + Send {
        self.tick();
        self.inner.land(repo, draft, line)
    }
    fn journal(
        &self,
        repo: RepoId,
        from: Lsn,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(Lsn, String, Vec<u8>)>>> + Send {
        self.tick();
        let entries = self.entries.clone();
        let fut = self.inner.journal(repo, from, limit);
        async move {
            let got = fut.await;
            if let Ok(v) = &got {
                entries.fetch_add(v.len() as u64, Ordering::Relaxed);
            }
            got
        }
    }
    fn journal_tip(&self, repo: RepoId) -> impl Future<Output = StoreResult<Option<Lsn>>> + Send {
        self.tick();
        self.inner.journal_tip(repo)
    }
    fn list_records(
        &self,
        repo: RepoId,
        state: RecordState,
        after: Option<ChangesetId>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(ChangesetId, ChangesetRecord)>>> + Send {
        self.tick();
        self.inner.list_records(repo, state, after, limit)
    }
    fn holder_rows(
        &self,
        repo: RepoId,
        object: ObjectId,
        after: Option<String>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<String>>> + Send {
        self.tick();
        self.inner.holder_rows(repo, object, after, limit)
    }
    fn list_all_workspaces(
        &self,
        repo: RepoId,
        after: &str,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send {
        self.tick();
        self.inner.list_all_workspaces(repo, after, limit)
    }
    fn protection(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> impl Future<Output = StoreResult<Option<Protection>>> + Send {
        self.tick();
        self.inner.protection(repo, id)
    }
    fn expired_records(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<(i64, ChangesetId)>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(i64, ChangesetId)>>> + Send {
        self.tick();
        self.inner.expired_records(repo, now_ms, after, limit)
    }
    fn held_objects(
        &self,
        repo: RepoId,
        holder: &str,
        after: Option<ObjectId>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<ObjectId>>> + Send {
        self.tick();
        self.inner.held_objects(repo, holder, after, limit)
    }
    fn expired_rows(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<TimedRow>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<TimedRow>>> + Send {
        self.tick();
        self.inner.expired_rows(repo, now_ms, after, limit)
    }
    fn doom_marks(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
    ) -> impl Future<Output = StoreResult<Vec<(ObjectId, DoomMark)>>> + Send {
        self.tick();
        self.inner.doom_marks(repo, ids)
    }
    fn old_doomed(
        &self,
        repo: RepoId,
        until_ms: i64,
        after: Option<(i64, ObjectId)>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(ObjectId, DoomMark)>>> + Send {
        self.tick();
        self.inner.old_doomed(repo, until_ms, after, limit)
    }
    fn lapsed_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        after: Option<(i64, String)>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send {
        self.tick();
        self.inner.lapsed_workspaces(repo, before_ms, after, limit)
    }
    fn scan_draft_paths(
        &self,
        repo: RepoId,
        line: &str,
        prefix: &RepoPath,
        after: Option<(RepoPath, String)>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(RepoPath, String)>>> + Send {
        self.tick();
        self.inner
            .scan_draft_paths(repo, line, prefix, after, limit)
    }
    fn workspace_draft_paths(
        &self,
        repo: RepoId,
        workspace: &str,
        after: Option<RepoPath>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<RepoPath>>> + Send {
        self.tick();
        self.inner
            .workspace_draft_paths(repo, workspace, after, limit)
    }
    fn gc_protection(&self, repo: RepoId) -> impl Future<Output = StoreResult<bool>> + Send {
        self.tick();
        self.inner.gc_protection(repo)
    }
    fn attestations_by_changeset(
        &self,
        repo: RepoId,
        changeset: ChangesetId,
        after: Option<(Lsn, u32)>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<AttestationRow>>> + Send {
        self.tick();
        self.inner
            .attestations_by_changeset(repo, changeset, after, limit)
    }
    fn attestations_by_line(
        &self,
        repo: RepoId,
        line: &str,
        from_pos: u64,
        to_pos: u64,
        after: Option<(u64, Lsn, u32)>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<AttestationRow>>> + Send {
        self.tick();
        self.inner
            .attestations_by_line(repo, line, from_pos, to_pos, after, limit)
    }
    fn land_checks(
        &self,
        repo: RepoId,
        line: &str,
        from_pos: u64,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<LandCheckRow>>> + Send {
        self.tick();
        self.inner.land_checks(repo, line, from_pos, limit)
    }
    fn review_queue(
        &self,
        repo: RepoId,
        line: &str,
        since: Lsn,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<LandingRow>>> + Send {
        let rows = self.rows.clone();
        let fut = self.inner.review_queue(repo, line, since, limit);
        async move {
            let got = fut.await;
            if let Ok(v) = &got {
                rows.fetch_add(v.len() as u64, Ordering::Relaxed);
            }
            got
        }
    }
}
