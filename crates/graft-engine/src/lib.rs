//! The repository engine: implements the `graft-proto` API traits over the
//! storage ports (`docs/design.md` §7, §8).
//!
//! Work items: G1.7 (write path and reads), G2.1 (workspaces and holders),
//! G2.7 (rebase and sync), G3.1 (land and policies), G3.7 (revert, stacks,
//! line merge).
//!
//! Phase 1 (G1.7) answers `CreateRepo`, the object API, `CreateChangeset`,
//! `ImportChangesets`, `GetChangesets`, `ResolveRef`, `ListRefs`,
//! `UpdateRef`, `Stat`, `ListTree`, `ReadFile`, `Diff` and `Watch`. Every
//! operation checks the caller's scopes first ([`access`]), then resolves
//! the repository, then validates, and writes only through one metadata
//! transaction per logical write, after the objects it references are
//! durable.

use std::sync::Arc;

use graft_index::Index;
use graft_model::{ChangesetId, Clock, ObjectId};
use graft_proto::CallContext;
use graft_proto::api::{AdminApi, ObjectApi, RepoApi, WorkspaceApi};
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_store::{MAX_PAGE, MetaStore, ObjectStore};

mod access;
mod diff;
mod errors;
pub mod journal;
mod objects;
pub mod policy;
mod read;
mod refs;
mod source;
mod write;

pub use diff::MAX_CONTEXT;
pub use refs::MAX_REF_NAME;
pub use write::MAX_REPO_NAME;

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
    /// Reject paths a common filesystem cannot hold (`portable_paths`).
    pub portable_paths: bool,
    /// Most ids per `GetObjects`, `HasObjects`, `PutObjects` or
    /// `GetChangesets` call.
    pub max_ids: u32,
    /// Frame bytes per `GetObjects` response; the rest is deferred.
    pub max_get_bytes: u64,
    /// Frame bytes per `PutObjects` call.
    pub max_put_bytes: u64,
    /// How long an upload holds its objects before a changeset must, ms.
    pub upload_lease_ms: u64,
    /// Page size when a request asks for 0.
    pub default_page: u32,
    /// Largest page of any list (at most the stores' `MAX_PAGE`).
    pub max_page: u32,
    /// Most changesets per `ImportChangesets` call.
    pub max_import: u32,
    /// Most path prefixes per `Diff`.
    pub max_diff_paths: u32,
    /// Most files per `Diff` page when patches are requested.
    pub max_patch_files: u32,
    /// Patch bytes per file when the request asks for 0.
    pub default_patch_file_bytes: u64,
    /// Largest per-file patch a request may ask for, bytes.
    pub max_patch_file_bytes: u64,
    /// Patch bytes per `Diff` response.
    pub max_patch_bytes: u64,
    /// Blobs larger than this get no patch, bytes.
    pub max_patch_input_bytes: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_blob_bytes: 64 << 20,
            max_changes: 100_000,
            max_inline_bytes: 4 << 20,
            land_attempts: 16,
            default_lease_ms: 2 * 3600 * 1000,
            portable_paths: false,
            max_ids: 1024,
            max_get_bytes: 64 << 20,
            max_put_bytes: 128 << 20,
            upload_lease_ms: 24 * 3600 * 1000,
            default_page: 100,
            max_page: MAX_PAGE,
            max_import: 1000,
            max_diff_paths: 64,
            max_patch_files: 256,
            default_patch_file_bytes: 64 << 10,
            max_patch_file_bytes: 1 << 20,
            max_patch_bytes: 4 << 20,
            max_patch_input_bytes: 1 << 20,
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

    /// The page size for a requested `limit`: the default for 0, otherwise
    /// clamped to the largest page.
    pub(crate) fn page_size(&self, limit: u32) -> u32 {
        let max = self.config.max_page.clamp(1, MAX_PAGE);
        match limit {
            0 => self.config.default_page.clamp(1, max),
            n => n.min(max),
        }
    }
}

impl<M, O> ObjectApi for Engine<M, O>
where
    M: MetaStore,
    O: ObjectStore,
{
    async fn get_objects(&self, cx: &CallContext, req: GetObjects) -> ApiResult<Objects> {
        self.do_get_objects(cx, req).await
    }
    async fn has_objects(&self, cx: &CallContext, req: HasObjects) -> ApiResult<Vec<ObjectId>> {
        self.do_has_objects(cx, req).await
    }
    async fn put_objects(&self, cx: &CallContext, req: PutObjects) -> ApiResult<Vec<ObjectId>> {
        self.do_put_objects(cx, req).await
    }
}

impl<M, O> RepoApi for Engine<M, O>
where
    M: MetaStore,
    O: ObjectStore,
{
    async fn create_changeset(&self, cx: &CallContext, req: CreateChangeset) -> ApiResult<Created> {
        self.do_create_changeset(cx, req).await
    }
    async fn get_changesets(&self, cx: &CallContext, req: GetChangesets) -> ApiResult<Changesets> {
        self.do_get_changesets(cx, req).await
    }
    async fn resolve_ref(&self, cx: &CallContext, req: ResolveRef) -> ApiResult<RefView> {
        self.do_resolve_ref(cx, req).await
    }
    async fn list_refs(&self, cx: &CallContext, req: ListRefs) -> ApiResult<Vec<RefView>> {
        self.do_list_refs(cx, req).await
    }
    async fn update_ref(&self, cx: &CallContext, req: UpdateRef) -> ApiResult<RefView> {
        self.do_update_ref(cx, req).await
    }
    async fn stat(&self, cx: &CallContext, req: PathAt) -> ApiResult<EntryView> {
        self.do_stat(cx, req).await
    }
    async fn list_tree(&self, cx: &CallContext, req: ListTree) -> ApiResult<Entries> {
        self.do_list_tree(cx, req).await
    }
    async fn read_file(&self, cx: &CallContext, req: PathAt) -> ApiResult<FileContent> {
        self.do_read_file(cx, req).await
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
    async fn diff(&self, cx: &CallContext, req: Diff) -> ApiResult<DiffResult> {
        self.do_diff(cx, req).await
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
    async fn watch(&self, cx: &CallContext, req: Watch) -> ApiResult<JournalPage> {
        self.do_watch(cx, req).await
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
    async fn create_repo(&self, cx: &CallContext, name: String) -> ApiResult<()> {
        self.do_create_repo(cx, name).await
    }
    async fn import_changesets(
        &self,
        cx: &CallContext,
        req: ImportChangesets,
    ) -> ApiResult<Vec<Created>> {
        self.do_import_changesets(cx, req).await
    }
    async fn gc(&self, _cx: &CallContext, _req: Gc) -> ApiResult<GcReport> {
        todo!("G2.9 via graft-gc")
    }
    async fn mint_token(&self, _cx: &CallContext, _req: MintToken) -> ApiResult<String> {
        todo!("G2.4 via graft-auth")
    }
}
