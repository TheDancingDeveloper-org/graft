//! Request and response types for every operation (`docs/design.md` §7.2).
//! The structs are plain, so every crate can build them with struct literals;
//! wire compatibility (adding fields without breaking peers) is the protobuf
//! layer's job (`proto/graft/v1`), not these types'. Enums that callers
//! match on stay `#[non_exhaustive]`.

use graft_model::{
    Author, BlobId, Change, Changeset, ChangesetId, EntryKind, Metadata, ObjectId, RepoPath,
    Timestamp, TreeId,
};

use crate::frame::Frame;

/// A page request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Page {
    /// Maximum items.
    pub limit: u32,
    /// Resume token from a previous response.
    pub token: Option<String>,
}

// ---- objects --------------------------------------------------------------

/// `GetObjects` request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GetObjects {
    /// Repository.
    pub repo: String,
    /// Wanted ids (at most 1,024).
    pub ids: Vec<ObjectId>,
}

/// `GetObjects` response. Partial results are normal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Objects {
    /// Found objects, as frames.
    pub found: Vec<(ObjectId, Frame)>,
    /// Not stored at this tier or above.
    pub missing: Vec<ObjectId>,
    /// Over the response size bound; ask again.
    pub deferred: Vec<ObjectId>,
}

/// `HasObjects` request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HasObjects {
    /// Repository.
    pub repo: String,
    /// Ids to check.
    pub ids: Vec<ObjectId>,
}

/// `PutObjects` request (blobs only).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PutObjects {
    /// Repository.
    pub repo: String,
    /// Blob frames.
    pub blobs: Vec<Frame>,
}

// ---- changesets -----------------------------------------------------------

/// `CreateChangeset` request ("commit").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateChangeset {
    /// Repository.
    pub repo: String,
    /// Parents; `parents[0]` is the base.
    pub parents: Vec<ChangesetId>,
    /// Changes against `parents[0]`.
    pub changes: Vec<Change>,
    /// Author.
    pub author: Author,
    /// Client-supplied time.
    pub authored_at: Timestamp,
    /// Message.
    pub message: String,
    /// Metadata.
    pub metadata: Metadata,
    /// Small contents sent inline (4 MiB total).
    pub inline_blobs: Vec<Frame>,
    /// Workspace whose head moves from `parents[0]` to the new id.
    pub workspace: Option<String>,
}

/// Lifecycle state of a changeset (`docs/design.md` §4.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftState {
    /// Held by a workspace or land attempt.
    Draft,
    /// On a line.
    Landed,
    /// Replaced by a rebased successor.
    Superseded,
    /// No holder; awaiting collection.
    Abandoned,
}

/// `CreateChangeset` response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Created {
    /// The new (or existing, on retry) id.
    pub id: ChangesetId,
    /// Its root tree.
    pub root: TreeId,
    /// Journal position.
    pub lsn: u64,
    /// State.
    pub state: DraftState,
}

/// `GetChangesets` request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GetChangesets {
    /// Repository.
    pub repo: String,
    /// Ids.
    pub ids: Vec<ChangesetId>,
}

/// A changeset with its server record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangesetView {
    /// Content.
    pub changeset: Changeset,
    /// Journal position.
    pub lsn: u64,
    /// State.
    pub state: DraftState,
    /// Successor, when superseded.
    pub superseded_by: Option<ChangesetId>,
}

/// `GetChangesets` response.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changesets {
    /// Found.
    pub found: Vec<ChangesetView>,
    /// Never existed here.
    pub missing: Vec<ChangesetId>,
    /// Collected (tombstoned).
    pub collected: Vec<ChangesetId>,
}

// ---- refs -----------------------------------------------------------------

/// Kind of ref (`docs/design.md` §4.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    /// Protected, first-parent fast-forward only, indexed.
    Line,
    /// Immutable once created.
    Tag,
    /// Unrestricted, tool-owned.
    Tool,
}

/// A ref's value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefView {
    /// Name.
    pub name: String,
    /// Target changeset.
    pub target: ChangesetId,
    /// Version, increasing on every move.
    pub version: u64,
    /// Kind.
    pub kind: RefKind,
}

/// `ResolveRef` request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolveRef {
    /// Repository.
    pub repo: String,
    /// Ref name.
    pub name: String,
}

/// `ListRefs` request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListRefs {
    /// Repository.
    pub repo: String,
    /// Name prefix.
    pub prefix: Option<String>,
    /// Paging.
    pub page: Page,
}

/// `UpdateRef` request (admin and tool refs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateRef {
    /// Repository.
    pub repo: String,
    /// Ref name.
    pub name: String,
    /// Expected current target; `None` means it must not exist.
    pub expected: Option<ChangesetId>,
    /// New target.
    pub new: ChangesetId,
    /// Kind, when creating.
    pub kind: RefKind,
}

/// `CreateLine` request: fork line `name` at `from`.
///
/// A compare-and-swap from absent to `from`, journalled as a `RefMoved`
/// entry in the same transaction. It never overwrites a ref: when `name`
/// exists the answer is `RefConflict { actual: Some(target) }`, and nothing
/// moves. A bad name is `InvalidChange { rule: "ref-name" }`; an unknown
/// `from` is `NotFound`. Needs `repo:<r>:admin` or `repo:<r>:land:<name>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateLine {
    /// Repository.
    pub repo: String,
    /// The new line's name.
    pub name: String,
    /// The changeset the line starts at.
    pub from: ChangesetId,
}

// ---- snapshot reads -------------------------------------------------------

/// `Stat`, `ReadFile` request: a path in a changeset's snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathAt {
    /// Repository.
    pub repo: String,
    /// Changeset.
    pub at: ChangesetId,
    /// Path.
    pub path: RepoPath,
}

/// An entry in a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryView {
    /// Path.
    pub path: RepoPath,
    /// Kind.
    pub kind: EntryKind,
    /// Id.
    pub id: ObjectId,
}

/// `ReadFile` response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileContent {
    /// Kind.
    pub kind: EntryKind,
    /// Blob id.
    pub id: BlobId,
    /// Bytes.
    pub data: Vec<u8>,
}

/// `ListTree` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListTree {
    /// Repository.
    pub repo: String,
    /// Changeset.
    pub at: ChangesetId,
    /// Directory; `None` is the root.
    pub path: Option<RepoPath>,
    /// Recurse (capped per page).
    pub recursive: bool,
    /// Paging.
    pub page: Page,
}

/// A page of entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entries {
    /// Entries.
    pub entries: Vec<EntryView>,
    /// Resume token.
    pub next: Option<String>,
}

// ---- history --------------------------------------------------------------

/// `Log` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Log {
    /// Repository.
    pub repo: String,
    /// Start (newest).
    pub from: ChangesetId,
    /// Stop before this changeset.
    pub until: Option<ChangesetId>,
    /// Restrict to changesets touching these paths or directories.
    pub paths: Vec<RepoPath>,
    /// Paging.
    pub page: Page,
}

/// A summary line of history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangesetSummary {
    /// Id.
    pub id: ChangesetId,
    /// Author.
    pub author: Author,
    /// Time.
    pub authored_at: Timestamp,
    /// First line of the message.
    pub subject: String,
    /// Line position, when landed.
    pub line_pos: Option<u64>,
}

/// A page of history.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    /// Newest first.
    pub changesets: Vec<ChangesetSummary>,
    /// Resume token.
    pub next: Option<String>,
}

/// `IsAncestor` / `MergeBase` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair {
    /// Repository.
    pub repo: String,
    /// First changeset (the ancestor, for `IsAncestor`).
    pub a: ChangesetId,
    /// Second changeset.
    pub b: ChangesetId,
}

/// Options for unified patches in `Diff`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PatchOptions {
    /// Context lines.
    pub context: u32,
    /// Cap per file, in bytes.
    pub max_bytes_per_file: u64,
}

/// `Diff` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diff {
    /// Repository.
    pub repo: String,
    /// From snapshot.
    pub from: ChangesetId,
    /// To snapshot.
    pub to: ChangesetId,
    /// Restrict to these prefixes.
    pub paths: Vec<RepoPath>,
    /// Include patches for text files.
    pub patch: Option<PatchOptions>,
    /// Paging.
    pub page: Page,
}

/// One file in a diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    /// The change.
    pub change: Change,
    /// Unified patch, when requested and text.
    pub patch: Option<String>,
    /// Whether the patch was truncated.
    pub truncated: bool,
}

/// `Diff` response.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffResult {
    /// Files, sorted by path.
    pub files: Vec<FileDiff>,
    /// Resume token.
    pub next: Option<String>,
}

/// `Blame` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blame {
    /// Repository.
    pub repo: String,
    /// Snapshot.
    pub at: ChangesetId,
    /// File.
    pub path: RepoPath,
    /// Optional 0-based line range.
    pub lines: Option<(u32, u32)>,
}

/// A run of lines with one origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameRange {
    /// First line (0-based).
    pub start: u32,
    /// One past the last line.
    pub end: u32,
    /// The changeset that introduced these lines.
    pub origin: ChangesetSummary,
}

/// `Blame` response.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlameResult {
    /// Ranges in order.
    pub ranges: Vec<BlameRange>,
}

// ---- rebase, revert, land -------------------------------------------------

/// How rebase treats a path both sides changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MergeMode {
    /// Any both-sides change conflicts.
    #[default]
    File,
    /// Three-way line merge for text files (decision D16).
    Line,
}

/// `Rebase` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rebase {
    /// Repository.
    pub repo: String,
    /// The changeset (or stack head) to move.
    pub changeset: ChangesetId,
    /// The new base.
    pub onto: ChangesetId,
    /// Merge mode.
    pub merge: MergeMode,
    /// Compute the result and its conflicts, but write nothing: no
    /// changeset, object, pin or journal entry (D27).
    pub dry_run: bool,
}

/// `Revert` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revert {
    /// Repository.
    pub repo: String,
    /// The changeset to invert.
    pub changeset: ChangesetId,
    /// Where to apply the inverse.
    pub onto: ChangesetId,
    /// Workspace to hold the result.
    pub workspace: Option<String>,
}

/// `Land` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Land {
    /// Repository.
    pub repo: String,
    /// Stack head to land.
    pub head: ChangesetId,
    /// Target line.
    pub line: String,
    /// Land the stack as one changeset.
    pub squash: bool,
    /// Merge mode for the rebase.
    pub merge: MergeMode,
    /// Give up after this many ms.
    pub deadline_ms: Option<u64>,
    /// Set when landing a merge resolution: the land records a merge point
    /// from this source (D29, `docs/design.md` §8.9).
    pub merged_from: Option<MergedFrom>,
}

/// The source of a merge landing (D29).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergedFrom {
    /// The source line.
    pub line: String,
    /// The source changeset merged; on `line`'s line index.
    pub changeset: ChangesetId,
}

/// `LineMerge` request: merge line `src` into line `dst` (D29,
/// `docs/design.md` §8.9). Only `dst` moves, by CAS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineMerge {
    /// Repository.
    pub repo: String,
    /// The source line, only read.
    pub src: String,
    /// The target line.
    pub dst: String,
    /// The source changeset to merge; `src`'s tip when absent.
    pub src_at: Option<ChangesetId>,
    /// Merge mode for the three-way merge.
    pub merge: MergeMode,
    /// Give up after this many ms.
    pub deadline_ms: Option<u64>,
    /// Message of the merge landing.
    pub message: Option<String>,
}

/// `LineMerge` success. The failures are `MergeConflict` (carrying `base`,
/// `tip` and the source changeset), `Contended` and `Rejected` in
/// [`crate::ApiError`], as for `Land`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LineMergeOutcome {
    /// The merge landed on `dst` and recorded a merge point.
    Merged {
        /// The merge landing.
        landed: ChangesetId,
        /// Line version after the move.
        line_version: u64,
        /// Line position of `landed`.
        pos: u64,
        /// Paths resolved by line merge.
        merged_paths: Vec<RepoPath>,
        /// The merge base the merge was computed from.
        base: ChangesetId,
        /// How the merge base was found; `Ancestry` when the bounded
        /// merge point search fell back to the ancestry walk.
        via: MergeBaseVia,
    },
    /// `dst` already contains the source changeset; nothing moved.
    UpToDate {
        /// `dst`'s tip.
        tip: ChangesetId,
    },
}

/// `LineMerges` request: a line's merge points, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineMerges {
    /// Repository.
    pub repo: String,
    /// The target line.
    pub dst: String,
    /// Only merges from this line.
    pub src: Option<String>,
    /// Paging (limit 100, max 1,000).
    pub page: Page,
}

/// One merge point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineMergePoint {
    /// The source line.
    pub src: String,
    /// The source changeset merged.
    pub src_changeset: ChangesetId,
    /// Its position on `src`.
    pub src_pos: u64,
    /// The merge landing on the target line.
    pub dst_changeset: ChangesetId,
    /// Its position on the target line.
    pub dst_pos: u64,
}

/// `LineMerges` response.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineMergePage {
    /// Newest first.
    pub merges: Vec<LineMergePoint>,
    /// Resume token.
    pub next: Option<String>,
}

/// Journal kind of a merge landing (D29).
pub const LINE_MERGED: &str = "LineMerged";

/// Body of a `LineMerged` journal entry, written in the merge landing's
/// CAS transaction beside its `RefMoved`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineMerged {
    /// The source line.
    pub src: String,
    /// The target line.
    pub dst: String,
    /// The source changeset merged.
    pub src_changeset: ChangesetId,
    /// Its position on `src`.
    pub src_pos: u64,
    /// Position of the merge landing on `dst`.
    pub dst_pos: u64,
}

/// How `MergeBase` found its answer (D29), so a capped search is visible.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MergeBaseVia {
    /// A recorded merge point.
    MergePoint,
    /// A line fork row (D23).
    Fork,
    /// The ancestry walk (§7.2).
    #[default]
    Ancestry,
}

/// `MergeBase` answer with how it was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeBaseResult {
    /// The merge base, if any.
    pub base: Option<ChangesetId>,
    /// How it was found.
    pub via: MergeBaseVia,
}

/// `Land` success. The failures are `Conflict`, `Contended` and `Rejected`
/// in [`crate::ApiError`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Landed {
    /// The line now contains the stack.
    Landed {
        /// The top landed changeset.
        landed: ChangesetId,
        /// Line version after the move.
        line_version: u64,
        /// Line position of `landed`.
        pos: u64,
        /// Whether a rebase was needed.
        rebased: bool,
        /// Paths resolved by line merge.
        merged_paths: Vec<RepoPath>,
    },
    /// Git-primary line: handed to the PR bridge (`docs/design.md` §11.3).
    Queued {
        /// The PR.
        pr_url: String,
    },
}

/// `Attest` request: CI reports a check result for a changeset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attest {
    /// Repository.
    pub repo: String,
    /// The changeset checked.
    pub changeset: ChangesetId,
    /// Check name, for example `ci/test`.
    pub check: String,
    /// Passed or not.
    pub passed: bool,
    /// Link to the run.
    pub url: String,
}

/// `Watch` request: read the journal from an LSN.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Watch {
    /// Repository.
    pub repo: String,
    /// First LSN wanted.
    pub from_lsn: u64,
    /// Only these entry kinds (empty = all).
    pub kinds: Vec<String>,
    /// Maximum entries per page.
    pub limit: u32,
    /// Long-poll wait when nothing is new, in ms.
    pub wait_ms: u32,
}

/// A journal entry as seen by watchers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JournalEntry {
    /// Position.
    pub lsn: u64,
    /// Entry kind, for example `RefMoved`.
    pub kind: String,
    /// The entry body, JSON-encoded.
    pub body: String,
}

/// A page of journal entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JournalPage {
    /// Entries in LSN order.
    pub entries: Vec<JournalEntry>,
    /// Next LSN to ask for.
    pub next_lsn: u64,
}

// ---- workspaces -----------------------------------------------------------

/// A workspace (`docs/design.md` §3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceView {
    /// Name.
    pub name: String,
    /// Owner subject.
    pub owner: String,
    /// Session (set by the caller's orchestrator), if any.
    pub session: Option<String>,
    /// Line it tracks.
    pub line: String,
    /// Base changeset on the line.
    pub base: ChangesetId,
    /// Top draft, if any.
    pub head: Option<ChangesetId>,
    /// Lease expiry, ms since the epoch.
    pub lease_until_ms: i64,
}

/// `OpenWorkspace` request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenWorkspace {
    /// Repository.
    pub repo: String,
    /// Name; generated when absent.
    pub name: Option<String>,
    /// Line to track.
    pub line: String,
    /// Base; the line tip when absent.
    pub at: Option<ChangesetId>,
    /// Lease length in ms.
    pub lease_ms: u64,
}

/// A request naming one workspace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceRef {
    /// Repository.
    pub repo: String,
    /// Name.
    pub name: String,
}

/// `SetHead` request (CAS).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetHead {
    /// The workspace.
    pub workspace: WorkspaceRef,
    /// Expected current head.
    pub expected: Option<ChangesetId>,
    /// New head.
    pub new: Option<ChangesetId>,
}

/// `Sync` response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Synced {
    /// The workspace after the sync.
    pub workspace: WorkspaceView,
    /// Paths the client must update in its working copy.
    pub changed_paths: Vec<RepoPath>,
}

/// A stack operation.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StackOp {
    /// New order, as draft ids bottom to top.
    Reorder(Vec<ChangesetId>),
    /// Squash these adjacent drafts into one.
    Squash(Vec<ChangesetId>),
    /// Split a draft by path sets.
    Split {
        /// The draft.
        draft: ChangesetId,
        /// Path groups, one per resulting draft.
        groups: Vec<Vec<RepoPath>>,
    },
    /// Drop a draft.
    Drop(ChangesetId),
}

// ---- admin ----------------------------------------------------------------

/// `ImportChangesets` request (admin): bulk import preserving parents.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportChangesets {
    /// Repository.
    pub repo: String,
    /// Changesets in topological order; roots allowed.
    pub changesets: Vec<Changeset>,
}

/// `Gc` request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Gc {
    /// Repository.
    pub repo: String,
    /// Report only.
    pub dry_run: bool,
}

/// `Gc` report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Changesets collected (or collectable, in a dry run).
    pub changesets: u64,
    /// Objects deleted (or doomed).
    pub objects: u64,
    /// Bytes reclaimed.
    pub bytes: u64,
}

/// `MintToken` request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MintToken {
    /// Subject.
    pub subject: String,
    /// Scopes; never broader than the caller's.
    pub scopes: Vec<String>,
    /// Lifetime in seconds.
    pub ttl_s: u64,
    /// Session (set by the caller's orchestrator, if any).
    pub session: Option<String>,
    /// Work item (set by the caller's orchestrator, if any).
    pub work_item: Option<String>,
}

// ---- conflict resolution (WI-2312) ----------------------------------------

/// The most bytes of rendered merge3 markers one [`ConflictView`] carries.
/// A path whose rendering is larger gets ids only
/// ([`ConflictReason::TooLarge`]); read its sides with `ReadFile` by id.
pub const MAX_CONFLICT_MARKER_BYTES: usize = 64 * 1024;

/// `GetConflicts` request: what a workspace's stack would conflict on if it
/// were synced onto `onto`. Read-only; nothing is stored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GetConflicts {
    /// The workspace.
    pub workspace: WorkspaceRef,
    /// Target; the line tip when absent.
    pub onto: Option<ChangesetId>,
    /// Page over conflicting paths, in path-byte order.
    pub page: Page,
}

/// Why a conflicting path has no rendered markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConflictReason {
    /// A side is not text.
    Binary,
    /// A side, or the rendering, is over the D16 or marker byte limit.
    TooLarge,
    /// The sides disagree on entry kind (file, symlink, submodule).
    Kind,
    /// One side deleted or added the path (no three-way text merge applies).
    Clash,
}

/// One side of a conflicting path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConflictSide {
    /// The entry's object id.
    pub id: ObjectId,
    /// The entry's kind.
    pub kind: EntryKind,
}

/// One conflicting path, by id, with merge3 markers when text-mergeable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictView {
    /// The path.
    pub path: RepoPath,
    /// The entry in the merge base; `None` when absent there.
    pub base: Option<ConflictSide>,
    /// The entry on the draft's side; `None` when the draft deleted it.
    pub ours: Option<ConflictSide>,
    /// The entry on `onto`; `None` when `onto` deleted it.
    pub theirs: Option<ConflictSide>,
    /// Whether all three sides are text within the D16 limits.
    pub text_mergeable: bool,
    /// `merge3` output with diff3 markers, at most
    /// [`MAX_CONFLICT_MARKER_BYTES`]. `None` when not text-mergeable or
    /// over the cap; `reason` says which.
    pub markers: Option<Vec<u8>>,
    /// Why `markers` is `None`.
    pub reason: Option<ConflictReason>,
}

/// `GetConflicts` response. Empty `paths` with no `next` means the stack
/// syncs cleanly onto `onto`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflicts {
    /// The target the conflicts were computed against.
    pub onto: ChangesetId,
    /// The bottom-most draft whose rebase failed, if any.
    pub draft: Option<ChangesetId>,
    /// This page of conflicting paths.
    pub paths: Vec<ConflictView>,
    /// Resume token for the next page.
    pub next: Option<String>,
}

/// The most input paths an `OverlappingDrafts` request may name.
pub const MAX_OVERLAP_PATHS: usize = 64;

/// The largest `OverlappingDrafts` page, in hits.
pub const MAX_OVERLAP_PAGE: u32 = 200;

/// `OverlappingDrafts` request: which other live workspaces on `line` have
/// open drafts changing paths under `paths` (`docs/notes/draft-overlap.md`
/// §4). Needs the `repo:<r>:drafts` scope.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OverlappingDrafts {
    /// Repository.
    pub repo: String,
    /// Line.
    pub line: String,
    /// Paths or directory prefixes, 1..=[`MAX_OVERLAP_PATHS`]. The root is
    /// not a path, so a whole-repository query cannot be expressed.
    pub paths: Vec<RepoPath>,
    /// Include the caller's own workspaces.
    pub include_own: bool,
    /// Page over hits (limit 1..=[`MAX_OVERLAP_PAGE`]), in index order.
    pub page: Page,
}

/// One `(path, workspace)` row of an overlap page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftHit {
    /// A path the workspace's stack changes.
    pub path: RepoPath,
    /// The workspace.
    pub workspace: String,
}

/// A workspace named by a [`DraftHit`]. Never its head id or contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftWorkspace {
    /// Workspace.
    pub workspace: String,
    /// Owner subject.
    pub owner: String,
    /// Session (set by the caller's orchestrator), if any.
    pub session: Option<String>,
    /// Work item (set by the caller's orchestrator), if any.
    pub work_item: Option<String>,
    /// How long ago its head draft was written, ms.
    pub draft_age_ms: u64,
    /// Lease expiry, ms since the epoch.
    pub lease_until_ms: i64,
}

/// `OverlappingDrafts` response. A page may be short, even empty, with a
/// `next` token: keep paging until `next` is absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DraftOverlap {
    /// Hits in index order: by input prefix, then `(path, workspace)`.
    pub hits: Vec<DraftHit>,
    /// One entry per workspace named in `hits`.
    pub workspaces: Vec<DraftWorkspace>,
    /// Resume token for the next page.
    pub next: Option<String>,
}

/// A caller-supplied resolution for one conflicting path.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Resolution {
    /// Put this content at the path.
    Put {
        /// File, exec or symlink.
        kind: EntryKind,
        /// The content.
        content: ResolvedContent,
    },
    /// Delete the path.
    Delete,
}

/// Where a [`Resolution::Put`]'s content comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResolvedContent {
    /// An already stored blob (uploaded with `PutObjects`, or one side's id).
    Blob(BlobId),
    /// Bytes sent inline, under the same caps as `CreateChangeset`'s inline
    /// blobs.
    Inline(Vec<u8>),
}

/// A path and its resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathResolution {
    /// The conflicting path.
    pub path: RepoPath,
    /// What to put there.
    pub resolution: Resolution,
}

/// `Resolve` request: rebase the workspace's stack onto `onto`, using the
/// caller's resolutions for the paths that conflict.
///
/// The engine runs it as **one transaction** (objects, rebased drafts, the
/// workspace move and its journal entry). The workspace moves `base → onto`
/// and `head → new head` **only by compare-and-swap** on `expected_head`;
/// the line is not touched until `Land`. The new ids are content-only and so
/// **deterministic**: the same stack, `onto` and resolutions give the same
/// head, and a retry after success returns it.
///
/// Every rewritten draft records `graft.rebased-from` as any rebase does.
/// The draft that took resolutions also records `graft.resolved-paths`, a
/// sorted JSON array of path strings; the engine defines that key next to
/// `graft.merged-paths` (`graft_engine::rebase`). Conflicting paths the
/// caller doesn't resolve are tried with the D16 line merge; any that still
/// conflict fail the call with `ApiError::Conflict` and nothing is written.
/// A resolution for a path that doesn't conflict is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolve {
    /// The workspace.
    pub workspace: WorkspaceRef,
    /// The target, as returned by `GetConflicts` or a `Conflict` error.
    pub onto: ChangesetId,
    /// The head the caller saw (CAS).
    pub expected_head: Option<ChangesetId>,
    /// Resolutions, one per path.
    pub resolutions: Vec<PathResolution>,
}

/// `Resolve` response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The workspace after the move (base `onto`, the new head).
    pub workspace: WorkspaceView,
    /// The new stack head.
    pub head: ChangesetId,
    /// Paths that took the caller's resolution, sorted.
    pub resolved_paths: Vec<RepoPath>,
    /// Paths merged by the D16 line merge, sorted.
    pub merged_paths: Vec<RepoPath>,
}
