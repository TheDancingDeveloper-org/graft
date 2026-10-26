//! A client backed by either gRPC or an engine opened in this process.

use std::sync::Arc;

use graft_model::{ChangesetId, ObjectId};
use graft_proto::api::{ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::context::CallContext;
use graft_proto::error::ApiResult;
use graft_proto::types::*;

use crate::local::LocalEngine;
use crate::{Endpoint, Remote};

/// The API used by a command: remote transport or local engine.
#[derive(Clone)]
pub enum Backend {
    /// gRPC to a server or node.
    Remote(Arc<Remote>),
    /// An in-process repository engine.
    Local {
        /// Engine opened for this process.
        engine: Arc<LocalEngine>,
        /// Repository name.
        repo: String,
    },
}

impl Backend {
    /// Wraps a remote client.
    pub fn remote(remote: Remote) -> Self {
        Self::Remote(Arc::new(remote))
    }

    /// Uses an engine opened in this process for `repo`.
    pub fn local(engine: Arc<LocalEngine>, repo: impl Into<String>) -> Self {
        Self::Local {
            engine,
            repo: repo.into(),
        }
    }

    /// The remote endpoint, if this is a remote backend.
    pub fn endpoint(&self) -> Option<&Endpoint> {
        match self {
            Self::Remote(remote) => Some(remote.endpoint()),
            Self::Local { .. } => None,
        }
    }

    fn local_context(repo: &str, cx: &CallContext) -> CallContext {
        crate::LocalRepo::context(repo, cx.request_id.clone())
    }
}

macro_rules! forward {
    ($self:ident.$method:ident($cx:ident $(, $arg:expr)*)) => {
        match $self {
            Backend::Remote(api) => api.$method($cx $(, $arg)*).await,
            Backend::Local { engine: api, repo } => {
                let local = Backend::local_context(repo, $cx);
                api.$method(&local $(, $arg)*).await
            },
        }
    };
}

impl ObjectApi for Backend {
    async fn get_objects(&self, cx: &CallContext, req: GetObjects) -> ApiResult<Objects> {
        forward!(self.get_objects(cx, req))
    }
    async fn has_objects(&self, cx: &CallContext, req: HasObjects) -> ApiResult<Vec<ObjectId>> {
        forward!(self.has_objects(cx, req))
    }
    async fn put_objects(&self, cx: &CallContext, req: PutObjects) -> ApiResult<Vec<ObjectId>> {
        forward!(self.put_objects(cx, req))
    }
}

impl RepoApi for Backend {
    async fn create_changeset(&self, cx: &CallContext, req: CreateChangeset) -> ApiResult<Created> {
        forward!(self.create_changeset(cx, req))
    }
    async fn get_changesets(&self, cx: &CallContext, req: GetChangesets) -> ApiResult<Changesets> {
        forward!(self.get_changesets(cx, req))
    }
    async fn resolve_ref(&self, cx: &CallContext, req: ResolveRef) -> ApiResult<RefView> {
        forward!(self.resolve_ref(cx, req))
    }
    async fn list_refs(&self, cx: &CallContext, req: ListRefs) -> ApiResult<Vec<RefView>> {
        forward!(self.list_refs(cx, req))
    }
    async fn update_ref(&self, cx: &CallContext, req: UpdateRef) -> ApiResult<RefView> {
        forward!(self.update_ref(cx, req))
    }
    async fn create_line(&self, cx: &CallContext, req: CreateLine) -> ApiResult<RefView> {
        forward!(self.create_line(cx, req))
    }
    async fn stat(&self, cx: &CallContext, req: PathAt) -> ApiResult<EntryView> {
        forward!(self.stat(cx, req))
    }
    async fn list_tree(&self, cx: &CallContext, req: ListTree) -> ApiResult<Entries> {
        forward!(self.list_tree(cx, req))
    }
    async fn read_file(&self, cx: &CallContext, req: PathAt) -> ApiResult<FileContent> {
        forward!(self.read_file(cx, req))
    }
    async fn log(&self, cx: &CallContext, req: Log) -> ApiResult<History> {
        forward!(self.log(cx, req))
    }
    async fn is_ancestor(&self, cx: &CallContext, req: Pair) -> ApiResult<bool> {
        forward!(self.is_ancestor(cx, req))
    }
    async fn merge_base(&self, cx: &CallContext, req: Pair) -> ApiResult<Option<ChangesetId>> {
        forward!(self.merge_base(cx, req))
    }
    async fn merge_base_via(&self, cx: &CallContext, req: Pair) -> ApiResult<MergeBaseResult> {
        forward!(self.merge_base_via(cx, req))
    }
    async fn diff(&self, cx: &CallContext, req: Diff) -> ApiResult<DiffResult> {
        forward!(self.diff(cx, req))
    }
    async fn blame(&self, cx: &CallContext, req: Blame) -> ApiResult<BlameResult> {
        forward!(self.blame(cx, req))
    }
    async fn rebase(&self, cx: &CallContext, req: Rebase) -> ApiResult<ChangesetId> {
        forward!(self.rebase(cx, req))
    }
    async fn revert(&self, cx: &CallContext, req: Revert) -> ApiResult<ChangesetId> {
        forward!(self.revert(cx, req))
    }
    async fn land(&self, cx: &CallContext, req: Land) -> ApiResult<Landed> {
        forward!(self.land(cx, req))
    }
    async fn line_merge(&self, cx: &CallContext, req: LineMerge) -> ApiResult<LineMergeOutcome> {
        forward!(self.line_merge(cx, req))
    }
    async fn line_merges(&self, cx: &CallContext, req: LineMerges) -> ApiResult<LineMergePage> {
        forward!(self.line_merges(cx, req))
    }
    async fn attest(&self, cx: &CallContext, req: Attest) -> ApiResult<()> {
        forward!(self.attest(cx, req))
    }
    async fn attest_v2(&self, cx: &CallContext, req: AttestV2) -> ApiResult<()> {
        forward!(self.attest_v2(cx, req))
    }
    async fn get_attestations(
        &self,
        cx: &CallContext,
        req: GetAttestations,
    ) -> ApiResult<AttestationPage> {
        forward!(self.get_attestations(cx, req))
    }
    async fn get_land_checks(
        &self,
        cx: &CallContext,
        req: GetLandChecks,
    ) -> ApiResult<LandCheckPage> {
        forward!(self.get_land_checks(cx, req))
    }
    async fn review_queue(&self, cx: &CallContext, req: ReviewQueue) -> ApiResult<ReviewQueuePage> {
        forward!(self.review_queue(cx, req))
    }
    async fn watch(&self, cx: &CallContext, req: Watch) -> ApiResult<JournalPage> {
        forward!(self.watch(cx, req))
    }
}

impl WorkspaceApi for Backend {
    async fn open_workspace(
        &self,
        cx: &CallContext,
        req: OpenWorkspace,
    ) -> ApiResult<WorkspaceView> {
        forward!(self.open_workspace(cx, req))
    }
    async fn get_workspace(&self, cx: &CallContext, req: WorkspaceRef) -> ApiResult<WorkspaceView> {
        forward!(self.get_workspace(cx, req))
    }
    async fn list_workspaces(
        &self,
        cx: &CallContext,
        repo: String,
        page: Page,
    ) -> ApiResult<Vec<WorkspaceView>> {
        forward!(self.list_workspaces(cx, repo, page))
    }
    async fn renew_lease(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        lease_ms: u64,
    ) -> ApiResult<WorkspaceView> {
        forward!(self.renew_lease(cx, req, lease_ms))
    }
    async fn set_head(&self, cx: &CallContext, req: SetHead) -> ApiResult<WorkspaceView> {
        forward!(self.set_head(cx, req))
    }
    async fn sync(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        onto: Option<ChangesetId>,
    ) -> ApiResult<Synced> {
        forward!(self.sync(cx, req, onto))
    }
    async fn stack(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        op: StackOp,
    ) -> ApiResult<WorkspaceView> {
        forward!(self.stack(cx, req, op))
    }
    async fn close_workspace(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        keep_drafts: bool,
    ) -> ApiResult<()> {
        forward!(self.close_workspace(cx, req, keep_drafts))
    }
    async fn get_conflicts(&self, cx: &CallContext, req: GetConflicts) -> ApiResult<Conflicts> {
        forward!(self.get_conflicts(cx, req))
    }
    async fn resolve(&self, cx: &CallContext, req: Resolve) -> ApiResult<Resolved> {
        forward!(self.resolve(cx, req))
    }
    async fn overlapping_drafts(
        &self,
        cx: &CallContext,
        req: OverlappingDrafts,
    ) -> ApiResult<DraftOverlap> {
        forward!(self.overlapping_drafts(cx, req))
    }
}
