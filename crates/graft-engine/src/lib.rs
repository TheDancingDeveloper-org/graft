//! The repository engine: implements the `graft-proto` API traits over the
//! storage ports (`docs/design.md` §7, §8).
//!
//! Work items: G1.7 (write path and reads), G2.1 (workspaces and holders),
//! G2.7 (rebase and sync), G3.1 (land and policies), G3.7 (revert, stacks,
//! line merge).
//!
//! v0.2 adds the generalised `Attest` (kinds `check`, `review` and
//! `approval`), `GetAttestations`, and land-checks rows with
//! `GetLandChecks` on lines whose checks are `PostLand` (`attest.rs`,
//! D25, D37). They record and never gate a land. `ReviewQueue`
//! (`review_queue.rs`) lists the landings of any line with no `review`
//! attestation yet, from the store's index, with summaries computed when
//! it is read.
//!
//! G3.7 adds `Revert` (`revert.rs`), the `Stack` operations (`stack.rs`),
//! line merge in `Rebase`, `Sync` and `Land` (decision D16, recorded in
//! [`MERGED_PATHS`]), and the `attested` policy's `attest_merged`.
//!
//! G3.11 adds the GC protection records of design §8.6 (`protect.rs`):
//! parent pins and expiries written with every record change, claims on
//! doomed objects before a write commits, and [`Engine::protect_repo`], the
//! backfill for repositories written before them.
//!
//! G3.1 adds `Land` and `Attest` with the `direct`, `attested` and
//! `queued` policies and path rules ([`policy`]); the land loop is in
//! `land.rs`.
//!
//! G2.7 adds file-level `Rebase` and `Sync` ([`rebase`]).
//!
//! G2.1 adds the workspace API (`OpenWorkspace`, `GetWorkspace`,
//! `ListWorkspaces`, `RenewLease`, `SetHead`, `CloseWorkspace`), drafts held
//! by workspaces, [`Engine::expire_leases`] and [`Engine::gauges`].
//!
//! Phase 1 (G1.7) answers `CreateRepo`, the object API, `CreateChangeset`,
//! `ImportChangesets`, `GetChangesets`, `ResolveRef`, `ListRefs`,
//! `UpdateRef`, `Stat`, `ListTree`, `ReadFile`, `Diff` and `Watch`; G1.10
//! adds `Log`, `IsAncestor`, `MergeBase` and `Blame` over `graft-index`. Every
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

/// Whether a planted bug is on; `false` without the `planted-bugs` feature.
#[cfg(feature = "planted-bugs")]
macro_rules! planted {
    ($bugs:expr, $bug:ident) => {
        $bugs.is_planted(graft_gc::planted::PlantedBug::$bug)
    };
}
#[cfg(not(feature = "planted-bugs"))]
macro_rules! planted {
    ($bugs:expr, $bug:ident) => {
        false
    };
}

mod access;
mod attest;
mod diff;
mod drafts;
mod errors;
mod gc;
mod history;
pub mod journal;
mod land;
mod merge;
mod objects;
pub mod policy;
mod protect;
mod read;
mod rebase;
mod refs;
mod resolve;
mod revert;
mod review_queue;
mod source;
mod stack;
mod workspace;
mod write;

pub use attest::MAX_ATTEST_BY;
pub use diff::MAX_CONTEXT;
pub use gc::GcSettings;
#[cfg(feature = "planted-bugs")]
pub use graft_gc::planted::{PlantedBug, PlantedBugs};
pub use land::{
    ATTESTATION, LAND_RESULT, MAX_ATTEST_URL, MAX_CHECK_NAME, REBASED_HOPS, Sleeper, ThreadSleeper,
};
pub use policy::{LandHooks, LineConfig};
pub use protect::{CLAIM_HOLDER, ProtectReport};
pub use rebase::{MERGED_PATHS, REBASED_FROM, RESOLVED_PATHS};
pub use refs::MAX_REF_NAME;
pub use revert::REVERTS;
pub use review_queue::{LAND_SUMMARY, LINES_UNAVAILABLE, MAX_REVIEW_SCAN, PATHS_UNAVAILABLE};
pub use workspace::{Gauges, MAX_WORKSPACE_NAME};
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
    /// Land-attempt lease on a candidate, ms; longer than any land deadline.
    pub land_lease_ms: u64,
    /// Land backoff base after a lost CAS, ms.
    pub land_backoff_base_ms: u64,
    /// Land backoff cap, ms.
    pub land_backoff_cap_ms: u64,
    /// Per-line land configuration.
    pub lines: Vec<LineConfig>,
    /// The configuration of lines `lines` does not name (its `name` is
    /// replaced by the line's).
    pub default_line: LineConfig,
    /// Default workspace lease, ms.
    pub default_lease_ms: u64,
    /// Longest workspace lease a request may ask for, ms.
    pub max_lease_ms: u64,
    /// Most drafts in one workspace stack (between its base and head).
    pub max_stack: u32,
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
    /// How long an abandoned or superseded changeset is kept before it may
    /// be collected, ms (design §8.6).
    pub draft_retention_ms: u64,
    /// How long a write's claim on a doomed object holds it, ms: longer
    /// than any request deadline, shorter than the GC grace period.
    pub claim_ms: u64,
    /// Page size when a request asks for 0.
    pub default_page: u32,
    /// Largest page of any list (at most the stores' `MAX_PAGE`).
    pub max_page: u32,
    /// Most changesets per `ImportChangesets` call.
    pub max_import: u32,
    /// Tree frame bytes one `ImportChangesets` call writes ahead of its
    /// commits before it commits the changesets they cover and carries on:
    /// memory stays bounded however wide the changed directories are.
    pub import_prewrite_bytes: usize,
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
    /// Most changed paths a `Sync` reports.
    pub max_sync_paths: u32,
    /// Most line hops one `MergeBase` follows through fork and merge rows
    /// before it falls back to the ancestry walk (D29, `H`).
    pub merge_hops: u32,
    /// Most indexed reads one `MergeBase` spends on merge points (D29, `R`).
    pub merge_reads: u32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_blob_bytes: 64 << 20,
            max_changes: 100_000,
            max_inline_bytes: 4 << 20,
            land_attempts: 16,
            land_lease_ms: 10 * 60 * 1000,
            land_backoff_base_ms: 2,
            land_backoff_cap_ms: 200,
            // Decision D22: every line, `main` included, lands `direct`.
            // D6 (`LineConfig::d6`) is the phase-2 opt-in.
            lines: Vec::new(),
            default_line: LineConfig::direct(""),
            default_lease_ms: 2 * 3600 * 1000,
            max_lease_ms: 24 * 3600 * 1000,
            max_stack: 1000,
            portable_paths: false,
            max_ids: 1024,
            max_get_bytes: 64 << 20,
            max_put_bytes: 128 << 20,
            upload_lease_ms: 24 * 3600 * 1000,
            draft_retention_ms: 7 * 24 * 3600 * 1000,
            claim_ms: 10 * 60 * 1000,
            default_page: 100,
            max_page: MAX_PAGE,
            max_import: 1000,
            import_prewrite_bytes: 64 << 20,
            max_diff_paths: 64,
            max_patch_files: 256,
            default_patch_file_bytes: 64 << 10,
            max_patch_file_bytes: 1 << 20,
            max_patch_bytes: 4 << 20,
            max_patch_input_bytes: 1 << 20,
            max_sync_paths: 100_000,
            merge_hops: 8,
            merge_reads: 64,
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
    hooks: Arc<dyn LandHooks>,
    sleeper: Arc<dyn Sleeper>,
    gc: gc::GcSettings,
    #[cfg(feature = "planted-bugs")]
    planted: graft_gc::planted::PlantedBugs,
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
            hooks: Arc::new(policy::NoHooks),
            sleeper: Arc::new(ThreadSleeper),
            gc: gc::GcSettings::default(),
            #[cfg(feature = "planted-bugs")]
            planted: graft_gc::planted::PlantedBugs::default(),
        }
    }

    /// Installs how `Gc` and [`Engine::gc_sweep`] run the collector.
    pub fn with_gc(mut self, gc: gc::GcSettings) -> Self {
        self.gc = gc;
        self
    }

    /// Shares the planted bugs `bugs` with this engine and the collectors
    /// it runs (test only).
    #[cfg(feature = "planted-bugs")]
    pub fn with_planted_bugs(mut self, bugs: graft_gc::planted::PlantedBugs) -> Self {
        self.planted = bugs;
        self
    }

    /// Installs the CI hooks `Land` and `Attest` call.
    pub fn with_land_hooks(mut self, hooks: Arc<dyn LandHooks>) -> Self {
        self.hooks = hooks;
        self
    }

    /// Installs the sleeper the land backoff uses.
    pub fn with_sleeper(mut self, sleeper: Arc<dyn Sleeper>) -> Self {
        self.sleeper = sleeper;
        self
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
    async fn create_line(&self, cx: &CallContext, req: CreateLine) -> ApiResult<RefView> {
        self.do_create_line(cx, req).await
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
    async fn log(&self, cx: &CallContext, req: Log) -> ApiResult<History> {
        self.do_log(cx, req).await
    }
    async fn is_ancestor(&self, cx: &CallContext, req: Pair) -> ApiResult<bool> {
        self.do_is_ancestor(cx, req).await
    }
    async fn merge_base(&self, cx: &CallContext, req: Pair) -> ApiResult<Option<ChangesetId>> {
        self.do_merge_base(cx, req).await
    }
    async fn merge_base_via(&self, cx: &CallContext, req: Pair) -> ApiResult<MergeBaseResult> {
        self.do_merge_base_via(cx, req).await
    }
    async fn diff(&self, cx: &CallContext, req: Diff) -> ApiResult<DiffResult> {
        self.do_diff(cx, req).await
    }
    async fn blame(&self, cx: &CallContext, req: Blame) -> ApiResult<BlameResult> {
        self.do_blame(cx, req).await
    }
    async fn rebase(&self, cx: &CallContext, req: Rebase) -> ApiResult<ChangesetId> {
        self.do_rebase(cx, req).await
    }
    async fn revert(&self, cx: &CallContext, req: Revert) -> ApiResult<ChangesetId> {
        self.do_revert(cx, req).await
    }
    async fn land(&self, cx: &CallContext, req: Land) -> ApiResult<Landed> {
        self.do_land(cx, req).await
    }
    async fn line_merge(&self, cx: &CallContext, req: LineMerge) -> ApiResult<LineMergeOutcome> {
        self.do_line_merge(cx, req).await
    }
    async fn line_merges(&self, cx: &CallContext, req: LineMerges) -> ApiResult<LineMergePage> {
        self.do_line_merges(cx, req).await
    }
    async fn attest(&self, cx: &CallContext, req: Attest) -> ApiResult<()> {
        self.do_attest(cx, req).await
    }
    async fn attest_v2(&self, cx: &CallContext, req: AttestV2) -> ApiResult<()> {
        self.do_attest_v2(cx, req).await
    }
    async fn get_attestations(
        &self,
        cx: &CallContext,
        req: GetAttestations,
    ) -> ApiResult<AttestationPage> {
        self.do_get_attestations(cx, req).await
    }
    async fn get_land_checks(
        &self,
        cx: &CallContext,
        req: GetLandChecks,
    ) -> ApiResult<LandCheckPage> {
        self.do_get_land_checks(cx, req).await
    }
    async fn review_queue(&self, cx: &CallContext, req: ReviewQueue) -> ApiResult<ReviewQueuePage> {
        self.do_review_queue(cx, req).await
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
        cx: &CallContext,
        req: OpenWorkspace,
    ) -> ApiResult<WorkspaceView> {
        self.do_open_workspace(cx, req).await
    }
    async fn get_workspace(&self, cx: &CallContext, req: WorkspaceRef) -> ApiResult<WorkspaceView> {
        self.do_get_workspace(cx, req).await
    }
    async fn list_workspaces(
        &self,
        cx: &CallContext,
        repo: String,
        page: Page,
    ) -> ApiResult<Vec<WorkspaceView>> {
        self.do_list_workspaces(cx, repo, page).await
    }
    async fn renew_lease(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        lease_ms: u64,
    ) -> ApiResult<WorkspaceView> {
        self.do_renew_lease(cx, req, lease_ms).await
    }
    async fn set_head(&self, cx: &CallContext, req: SetHead) -> ApiResult<WorkspaceView> {
        self.do_set_head(cx, req).await
    }
    async fn sync(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        onto: Option<ChangesetId>,
    ) -> ApiResult<Synced> {
        self.do_sync(cx, req, onto).await
    }
    async fn stack(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        op: StackOp,
    ) -> ApiResult<WorkspaceView> {
        self.do_stack(cx, req, op).await
    }
    async fn close_workspace(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        keep_drafts: bool,
    ) -> ApiResult<()> {
        self.do_close_workspace(cx, req, keep_drafts).await
    }

    async fn get_conflicts(
        &self,
        cx: &graft_proto::CallContext,
        req: graft_proto::types::GetConflicts,
    ) -> graft_proto::error::ApiResult<graft_proto::types::Conflicts> {
        self.do_get_conflicts(cx, req).await
    }

    async fn resolve(
        &self,
        cx: &graft_proto::CallContext,
        req: graft_proto::types::Resolve,
    ) -> graft_proto::error::ApiResult<graft_proto::types::Resolved> {
        self.do_resolve(cx, req).await
    }

    async fn overlapping_drafts(
        &self,
        cx: &graft_proto::CallContext,
        req: graft_proto::types::OverlappingDrafts,
    ) -> graft_proto::error::ApiResult<graft_proto::types::DraftOverlap> {
        self.do_overlapping_drafts(cx, req).await
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
    async fn gc(&self, cx: &CallContext, req: Gc) -> ApiResult<GcReport> {
        self.do_gc(cx, req).await
    }
    async fn mint_token(&self, _cx: &CallContext, _req: MintToken) -> ApiResult<String> {
        todo!("G2.4 via graft-auth")
    }
}
