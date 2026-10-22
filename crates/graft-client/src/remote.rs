//! The remote API client over gRPC with gzip (decision D4).

use graft_model::{ChangesetId, ObjectId};
use graft_proto::CallContext;
use graft_proto::api::{ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::error::ApiResult;
use graft_proto::types::*;

use crate::Endpoint;

/// A connection to a server or node.
#[derive(Debug)]
pub struct Remote {
    endpoint: Endpoint,
}

impl Remote {
    /// Connects and checks protocol version binding.
    pub async fn connect(endpoint: Endpoint) -> ApiResult<Self> {
        Ok(Self { endpoint })
    }

    /// The endpoint in use.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

impl ObjectApi for Remote {
    async fn get_objects(&self, _cx: &CallContext, _req: GetObjects) -> ApiResult<Objects> {
        todo!("G1.11")
    }
    async fn has_objects(&self, _cx: &CallContext, _req: HasObjects) -> ApiResult<Vec<ObjectId>> {
        todo!("G1.11")
    }
    async fn put_objects(&self, _cx: &CallContext, _req: PutObjects) -> ApiResult<Vec<ObjectId>> {
        todo!("G1.11")
    }
}

impl RepoApi for Remote {
    async fn create_changeset(
        &self,
        _cx: &CallContext,
        _req: CreateChangeset,
    ) -> ApiResult<Created> {
        todo!("G1.11")
    }
    async fn get_changesets(
        &self,
        _cx: &CallContext,
        _req: GetChangesets,
    ) -> ApiResult<Changesets> {
        todo!("G1.11")
    }
    async fn resolve_ref(&self, _cx: &CallContext, _req: ResolveRef) -> ApiResult<RefView> {
        todo!("G1.11")
    }
    async fn list_refs(&self, _cx: &CallContext, _req: ListRefs) -> ApiResult<Vec<RefView>> {
        todo!("G1.11")
    }
    async fn update_ref(&self, _cx: &CallContext, _req: UpdateRef) -> ApiResult<RefView> {
        todo!("G1.11")
    }
    async fn stat(&self, _cx: &CallContext, _req: PathAt) -> ApiResult<EntryView> {
        todo!("G1.11")
    }
    async fn list_tree(&self, _cx: &CallContext, _req: ListTree) -> ApiResult<Entries> {
        todo!("G1.11")
    }
    async fn read_file(&self, _cx: &CallContext, _req: PathAt) -> ApiResult<FileContent> {
        todo!("G1.11")
    }
    async fn log(&self, _cx: &CallContext, _req: Log) -> ApiResult<History> {
        todo!("G1.11")
    }
    async fn is_ancestor(&self, _cx: &CallContext, _req: Pair) -> ApiResult<bool> {
        todo!("G1.11")
    }
    async fn merge_base(&self, _cx: &CallContext, _req: Pair) -> ApiResult<Option<ChangesetId>> {
        todo!("G1.11")
    }
    async fn diff(&self, _cx: &CallContext, _req: Diff) -> ApiResult<DiffResult> {
        todo!("G1.11")
    }
    async fn blame(&self, _cx: &CallContext, _req: Blame) -> ApiResult<BlameResult> {
        todo!("G1.11")
    }
    async fn rebase(&self, _cx: &CallContext, _req: Rebase) -> ApiResult<ChangesetId> {
        todo!("G1.11")
    }
    async fn revert(&self, _cx: &CallContext, _req: Revert) -> ApiResult<ChangesetId> {
        todo!("G1.11")
    }
    async fn land(&self, _cx: &CallContext, _req: Land) -> ApiResult<Landed> {
        todo!("G1.11")
    }
    async fn attest(&self, _cx: &CallContext, _req: Attest) -> ApiResult<()> {
        todo!("G1.11")
    }
    async fn watch(&self, _cx: &CallContext, _req: Watch) -> ApiResult<JournalPage> {
        todo!("G1.11")
    }
}

impl WorkspaceApi for Remote {
    async fn open_workspace(
        &self,
        _cx: &CallContext,
        _req: OpenWorkspace,
    ) -> ApiResult<WorkspaceView> {
        todo!("G2.5")
    }
    async fn get_workspace(
        &self,
        _cx: &CallContext,
        _req: WorkspaceRef,
    ) -> ApiResult<WorkspaceView> {
        todo!("G2.5")
    }
    async fn list_workspaces(
        &self,
        _cx: &CallContext,
        _repo: String,
        _page: Page,
    ) -> ApiResult<Vec<WorkspaceView>> {
        todo!("G2.5")
    }
    async fn renew_lease(
        &self,
        _cx: &CallContext,
        _req: WorkspaceRef,
        _lease_ms: u64,
    ) -> ApiResult<WorkspaceView> {
        todo!("G2.5")
    }
    async fn set_head(&self, _cx: &CallContext, _req: SetHead) -> ApiResult<WorkspaceView> {
        todo!("G2.5")
    }
    async fn sync(
        &self,
        _cx: &CallContext,
        _req: WorkspaceRef,
        _onto: Option<ChangesetId>,
    ) -> ApiResult<Synced> {
        todo!("G2.5")
    }
    async fn stack(
        &self,
        _cx: &CallContext,
        _req: WorkspaceRef,
        _op: StackOp,
    ) -> ApiResult<WorkspaceView> {
        todo!("G3.5")
    }
    async fn close_workspace(
        &self,
        _cx: &CallContext,
        _req: WorkspaceRef,
        _keep_drafts: bool,
    ) -> ApiResult<()> {
        todo!("G2.5")
    }
}
