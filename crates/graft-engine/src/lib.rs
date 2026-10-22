//! The repository engine: implements the `graft-proto` API traits over the
//! storage ports (`docs/design.md` §7, §8).
//!
//! Work items: G1.7 (write path and reads), G2.1 (workspaces and holders),
//! G2.7 (rebase and sync), G3.1 (land and policies), G3.7 (revert, stacks,
//! line merge).

use std::sync::Arc;

use graft_index::Index;
use graft_model::{ChangesetId, Clock, ObjectId};
use graft_proto::CallContext;
use graft_proto::api::{AdminApi, ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_store::{MetaStore, ObjectStore};

pub mod policy;

/// Engine limits and defaults (`docs/design.md` §4, §7, §8).
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Largest accepted blob, bytes.
    pub max_blob_bytes: u64,
    /// Most changes per changeset.
    pub max_changes: u32,
    /// Inline blob budget per `CreateChangeset`, bytes.
    pub max_inline_bytes: u64,
    /// Land attempt budget.
    pub land_attempts: u32,
    /// Default workspace lease, ms.
    pub default_lease_ms: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_blob_bytes: 64 << 20,
            max_changes: 100_000,
            max_inline_bytes: 4 << 20,
            land_attempts: 16,
            default_lease_ms: 2 * 3600 * 1000,
        }
    }
}

/// The engine over a metadata store `M` and an object store `O`.
pub struct Engine<M, O> {
    index: Index<Arc<M>, Arc<O>>,
    meta: Arc<M>,
    objects: Arc<O>,
    clock: Arc<dyn Clock>,
    config: EngineConfig,
}

impl<M, O> Engine<M, O>
where
    M: MetaStore,
    O: ObjectStore,
{
    /// Builds an engine.
    pub fn new(meta: Arc<M>, objects: Arc<O>, clock: Arc<dyn Clock>, config: EngineConfig) -> Self {
        let index = Index::new(meta.clone(), objects.clone());
        Self {
            index,
            meta,
            objects,
            clock,
            config,
        }
    }

    /// The configuration.
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// The derived-data index.
    pub fn index(&self) -> &Index<Arc<M>, Arc<O>> {
        &self.index
    }

    /// The stores.
    pub fn stores(&self) -> (&M, &O) {
        (&self.meta, &self.objects)
    }

    /// The lease clock.
    pub fn clock(&self) -> &dyn Clock {
        self.clock.as_ref()
    }
}

impl<M, O> ObjectApi for Engine<M, O>
where
    M: MetaStore,
    O: ObjectStore,
{
    async fn get_objects(&self, _cx: &CallContext, _req: GetObjects) -> ApiResult<Objects> {
        todo!("G1.7")
    }
    async fn has_objects(&self, _cx: &CallContext, _req: HasObjects) -> ApiResult<Vec<ObjectId>> {
        todo!("G1.7")
    }
    async fn put_objects(&self, _cx: &CallContext, _req: PutObjects) -> ApiResult<Vec<ObjectId>> {
        todo!("G1.7")
    }
}

impl<M, O> RepoApi for Engine<M, O>
where
    M: MetaStore,
    O: ObjectStore,
{
    async fn create_changeset(
        &self,
        _cx: &CallContext,
        _req: CreateChangeset,
    ) -> ApiResult<Created> {
        todo!("G1.7")
    }
    async fn get_changesets(
        &self,
        _cx: &CallContext,
        _req: GetChangesets,
    ) -> ApiResult<Changesets> {
        todo!("G1.7")
    }
    async fn resolve_ref(&self, _cx: &CallContext, _req: ResolveRef) -> ApiResult<RefView> {
        todo!("G1.7")
    }
    async fn list_refs(&self, _cx: &CallContext, _req: ListRefs) -> ApiResult<Vec<RefView>> {
        todo!("G1.7")
    }
    async fn update_ref(&self, _cx: &CallContext, _req: UpdateRef) -> ApiResult<RefView> {
        todo!("G1.7")
    }
    async fn stat(&self, _cx: &CallContext, _req: PathAt) -> ApiResult<EntryView> {
        todo!("G1.7")
    }
    async fn list_tree(&self, _cx: &CallContext, _req: ListTree) -> ApiResult<Entries> {
        todo!("G1.7")
    }
    async fn read_file(&self, _cx: &CallContext, _req: PathAt) -> ApiResult<FileContent> {
        todo!("G1.7")
    }
    async fn log(&self, _cx: &CallContext, _req: Log) -> ApiResult<History> {
        todo!("G1.10 via graft-index")
    }
    async fn is_ancestor(&self, _cx: &CallContext, _req: Pair) -> ApiResult<bool> {
        todo!("G1.10 via graft-index")
    }
    async fn merge_base(&self, _cx: &CallContext, _req: Pair) -> ApiResult<Option<ChangesetId>> {
        todo!("G1.10 via graft-index")
    }
    async fn diff(&self, _cx: &CallContext, _req: Diff) -> ApiResult<DiffResult> {
        todo!("G1.7")
    }
    async fn blame(&self, _cx: &CallContext, _req: Blame) -> ApiResult<BlameResult> {
        todo!("G1.10 via graft-index")
    }
    async fn rebase(&self, _cx: &CallContext, _req: Rebase) -> ApiResult<ChangesetId> {
        todo!("G2.7")
    }
    async fn revert(&self, _cx: &CallContext, _req: Revert) -> ApiResult<ChangesetId> {
        todo!("G3.7")
    }
    async fn land(&self, _cx: &CallContext, _req: Land) -> ApiResult<Landed> {
        todo!("G3.1")
    }
    async fn attest(&self, _cx: &CallContext, _req: Attest) -> ApiResult<()> {
        todo!("G3.1")
    }
    async fn watch(&self, _cx: &CallContext, _req: Watch) -> ApiResult<JournalPage> {
        todo!("G1.7")
    }
}

impl<M, O> WorkspaceApi for Engine<M, O>
where
    M: MetaStore,
    O: ObjectStore,
{
    async fn open_workspace(
        &self,
        _cx: &CallContext,
        _req: OpenWorkspace,
    ) -> ApiResult<WorkspaceView> {
        todo!("G2.1")
    }
    async fn get_workspace(
        &self,
        _cx: &CallContext,
        _req: WorkspaceRef,
    ) -> ApiResult<WorkspaceView> {
        todo!("G2.1")
    }
    async fn list_workspaces(
        &self,
        _cx: &CallContext,
        _repo: String,
        _page: Page,
    ) -> ApiResult<Vec<WorkspaceView>> {
        todo!("G2.1")
    }
    async fn renew_lease(
        &self,
        _cx: &CallContext,
        _req: WorkspaceRef,
        _lease_ms: u64,
    ) -> ApiResult<WorkspaceView> {
        todo!("G2.1")
    }
    async fn set_head(&self, _cx: &CallContext, _req: SetHead) -> ApiResult<WorkspaceView> {
        todo!("G2.1")
    }
    async fn sync(
        &self,
        _cx: &CallContext,
        _req: WorkspaceRef,
        _onto: Option<ChangesetId>,
    ) -> ApiResult<Synced> {
        todo!("G2.7")
    }
    async fn stack(
        &self,
        _cx: &CallContext,
        _req: WorkspaceRef,
        _op: StackOp,
    ) -> ApiResult<WorkspaceView> {
        todo!("G3.7")
    }
    async fn close_workspace(
        &self,
        _cx: &CallContext,
        _req: WorkspaceRef,
        _keep_drafts: bool,
    ) -> ApiResult<()> {
        todo!("G2.1")
    }
}

impl<M, O> AdminApi for Engine<M, O>
where
    M: MetaStore,
    O: ObjectStore,
{
    async fn create_repo(&self, _cx: &CallContext, _name: String) -> ApiResult<()> {
        todo!("G1.7")
    }
    async fn import_changesets(
        &self,
        _cx: &CallContext,
        _req: ImportChangesets,
    ) -> ApiResult<Vec<Created>> {
        todo!("G1.7")
    }
    async fn gc(&self, _cx: &CallContext, _req: Gc) -> ApiResult<GcReport> {
        todo!("G2.9 via graft-gc")
    }
    async fn mint_token(&self, _cx: &CallContext, _req: MintToken) -> ApiResult<String> {
        todo!("G2.4 via graft-auth")
    }
}
