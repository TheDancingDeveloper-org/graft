//! The API traits (`docs/design.md` §5.2, §7.2).
//!
//! Streaming operations (`GetObjects`, `Watch`) are expressed as pages here;
//! the gRPC transport maps them to server streams.

use std::future::Future;

use graft_model::{ChangesetId, ObjectId};

use crate::context::CallContext;
use crate::error::ApiResult;
use crate::types::*;

/// Object operations, answered identically by every tier.
pub trait ObjectApi: Send + Sync {
    /// Returns what it has; misses are listed, not errors.
    fn get_objects(
        &self,
        cx: &CallContext,
        req: GetObjects,
    ) -> impl Future<Output = ApiResult<Objects>> + Send;
    /// Returns the ids not stored.
    fn has_objects(
        &self,
        cx: &CallContext,
        req: HasObjects,
    ) -> impl Future<Output = ApiResult<Vec<ObjectId>>> + Send;
    /// Stores blobs; idempotent.
    fn put_objects(
        &self,
        cx: &CallContext,
        req: PutObjects,
    ) -> impl Future<Output = ApiResult<Vec<ObjectId>>> + Send;
}

/// Repository operations, answered by the server (the node forwards them).
pub trait RepoApi: Send + Sync {
    /// Creates a changeset ("commit"); idempotent.
    fn create_changeset(
        &self,
        cx: &CallContext,
        req: CreateChangeset,
    ) -> impl Future<Output = ApiResult<Created>> + Send;
    /// Fetches changesets with their records.
    fn get_changesets(
        &self,
        cx: &CallContext,
        req: GetChangesets,
    ) -> impl Future<Output = ApiResult<Changesets>> + Send;
    /// Resolves a ref.
    fn resolve_ref(
        &self,
        cx: &CallContext,
        req: ResolveRef,
    ) -> impl Future<Output = ApiResult<RefView>> + Send;
    /// Lists refs.
    fn list_refs(
        &self,
        cx: &CallContext,
        req: ListRefs,
    ) -> impl Future<Output = ApiResult<Vec<RefView>>> + Send;
    /// Moves an admin or tool ref by CAS.
    fn update_ref(
        &self,
        cx: &CallContext,
        req: UpdateRef,
    ) -> impl Future<Output = ApiResult<RefView>> + Send;
    /// Forks a line at a changeset: CAS from absent, never an overwrite.
    /// See [`CreateLine`] for the contract. Implementations that cannot
    /// create lines keep the default, which answers `Unavailable`.
    fn create_line(
        &self,
        cx: &CallContext,
        req: CreateLine,
    ) -> impl Future<Output = ApiResult<RefView>> + Send {
        let _ = (cx, req);
        // Not served here at all: back off before asking again.
        async {
            Err(crate::ApiError::Unavailable {
                retry_after_ms: 60_000,
            })
        }
    }
    /// Stats a path.
    fn stat(
        &self,
        cx: &CallContext,
        req: PathAt,
    ) -> impl Future<Output = ApiResult<EntryView>> + Send;
    /// Lists a directory.
    fn list_tree(
        &self,
        cx: &CallContext,
        req: ListTree,
    ) -> impl Future<Output = ApiResult<Entries>> + Send;
    /// Reads a file.
    fn read_file(
        &self,
        cx: &CallContext,
        req: PathAt,
    ) -> impl Future<Output = ApiResult<FileContent>> + Send;
    /// First-parent history.
    fn log(&self, cx: &CallContext, req: Log) -> impl Future<Output = ApiResult<History>> + Send;
    /// Whether `a` is an ancestor of `b`.
    fn is_ancestor(
        &self,
        cx: &CallContext,
        req: Pair,
    ) -> impl Future<Output = ApiResult<bool>> + Send;
    /// A merge base of `a` and `b`, if any.
    fn merge_base(
        &self,
        cx: &CallContext,
        req: Pair,
    ) -> impl Future<Output = ApiResult<Option<ChangesetId>>> + Send;
    /// `MergeBase` with how it was found (D29): merge points first, then
    /// the ancestry walk. The default answers from `merge_base`, via
    /// ancestry.
    fn merge_base_via(
        &self,
        cx: &CallContext,
        req: Pair,
    ) -> impl Future<Output = ApiResult<MergeBaseResult>> + Send {
        async move {
            Ok(MergeBaseResult {
                base: self.merge_base(cx, req).await?,
                via: MergeBaseVia::Ancestry,
            })
        }
    }
    /// Diff two snapshots.
    fn diff(
        &self,
        cx: &CallContext,
        req: Diff,
    ) -> impl Future<Output = ApiResult<DiffResult>> + Send;
    /// Line blame.
    fn blame(
        &self,
        cx: &CallContext,
        req: Blame,
    ) -> impl Future<Output = ApiResult<BlameResult>> + Send;
    /// Rebase a changeset or stack.
    fn rebase(
        &self,
        cx: &CallContext,
        req: Rebase,
    ) -> impl Future<Output = ApiResult<ChangesetId>> + Send;
    /// Create an inverse changeset.
    fn revert(
        &self,
        cx: &CallContext,
        req: Revert,
    ) -> impl Future<Output = ApiResult<ChangesetId>> + Send;
    /// Land a stack on a line.
    fn land(&self, cx: &CallContext, req: Land) -> impl Future<Output = ApiResult<Landed>> + Send;
    /// Merge one line into another (D29, `docs/design.md` §8.9).
    /// Implementations that cannot merge lines keep the default, which
    /// answers `Unavailable`.
    fn line_merge(
        &self,
        cx: &CallContext,
        req: LineMerge,
    ) -> impl Future<Output = ApiResult<LineMergeOutcome>> + Send {
        let _ = (cx, req);
        async {
            Err(crate::ApiError::Unavailable {
                retry_after_ms: 60_000,
            })
        }
    }
    /// A line's merge points, newest first (D29). The default answers
    /// `Unavailable`.
    fn line_merges(
        &self,
        cx: &CallContext,
        req: LineMerges,
    ) -> impl Future<Output = ApiResult<LineMergePage>> + Send {
        let _ = (cx, req);
        async {
            Err(crate::ApiError::Unavailable {
                retry_after_ms: 60_000,
            })
        }
    }
    /// Record a check result.
    fn attest(&self, cx: &CallContext, req: Attest) -> impl Future<Output = ApiResult<()>> + Send;
    /// Read the journal (long-poll page).
    fn watch(
        &self,
        cx: &CallContext,
        req: Watch,
    ) -> impl Future<Output = ApiResult<JournalPage>> + Send;
}

/// Workspace operations.
pub trait WorkspaceApi: Send + Sync {
    /// Opens a workspace.
    fn open_workspace(
        &self,
        cx: &CallContext,
        req: OpenWorkspace,
    ) -> impl Future<Output = ApiResult<WorkspaceView>> + Send;
    /// Gets one workspace.
    fn get_workspace(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
    ) -> impl Future<Output = ApiResult<WorkspaceView>> + Send;
    /// Lists the caller's workspaces.
    fn list_workspaces(
        &self,
        cx: &CallContext,
        repo: String,
        page: Page,
    ) -> impl Future<Output = ApiResult<Vec<WorkspaceView>>> + Send;
    /// Extends the lease.
    fn renew_lease(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        lease_ms: u64,
    ) -> impl Future<Output = ApiResult<WorkspaceView>> + Send;
    /// Moves the head by CAS.
    fn set_head(
        &self,
        cx: &CallContext,
        req: SetHead,
    ) -> impl Future<Output = ApiResult<WorkspaceView>> + Send;
    /// Rebases the stack onto the line tip (or `onto`).
    fn sync(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        onto: Option<ChangesetId>,
    ) -> impl Future<Output = ApiResult<Synced>> + Send;
    /// Applies a stack operation.
    fn stack(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        op: StackOp,
    ) -> impl Future<Output = ApiResult<WorkspaceView>> + Send;
    /// Closes a workspace.
    fn close_workspace(
        &self,
        cx: &CallContext,
        req: WorkspaceRef,
        keep_drafts: bool,
    ) -> impl Future<Output = ApiResult<()>> + Send;
    /// Reports, per path, what the stack would conflict on if synced onto
    /// `onto` (the line tip by default): base, ours and theirs ids and
    /// bounded merge3 markers. Read-only and paged.
    fn get_conflicts(
        &self,
        cx: &CallContext,
        req: GetConflicts,
    ) -> impl Future<Output = ApiResult<Conflicts>> + Send;
    /// Rebases the stack onto `onto` with the caller's resolutions, in one
    /// transaction, moving the workspace only by CAS on `expected_head`.
    /// See [`Resolve`] for the contract.
    fn resolve(
        &self,
        cx: &CallContext,
        req: Resolve,
    ) -> impl Future<Output = ApiResult<Resolved>> + Send;
    /// Which other live workspaces have open drafts on these paths. See
    /// [`OverlappingDrafts`] for the contract. Implementations without the
    /// draft path index keep the default, which answers `Unavailable`.
    fn overlapping_drafts(
        &self,
        cx: &CallContext,
        req: OverlappingDrafts,
    ) -> impl Future<Output = ApiResult<DraftOverlap>> + Send {
        let _ = (cx, req);
        // Not served here at all: back off before asking again.
        async {
            Err(crate::ApiError::Unavailable {
                retry_after_ms: 60_000,
            })
        }
    }
}

/// Admin operations.
pub trait AdminApi: Send + Sync {
    /// Creates a repository.
    fn create_repo(
        &self,
        cx: &CallContext,
        name: String,
    ) -> impl Future<Output = ApiResult<()>> + Send;
    /// Bulk import preserving parents.
    fn import_changesets(
        &self,
        cx: &CallContext,
        req: ImportChangesets,
    ) -> impl Future<Output = ApiResult<Vec<Created>>> + Send;
    /// Runs or previews collection.
    fn gc(&self, cx: &CallContext, req: Gc) -> impl Future<Output = ApiResult<GcReport>> + Send;
    /// Mints a token no broader than the caller's.
    fn mint_token(
        &self,
        cx: &CallContext,
        req: MintToken,
    ) -> impl Future<Output = ApiResult<String>> + Send;
}
