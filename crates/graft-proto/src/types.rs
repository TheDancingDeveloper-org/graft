//! Request and response types for every operation (`docs/design.md` §7.2).
//! Each is `#[non_exhaustive]` so fields can be added compatibly.

use graft_model::{
    Author, BlobId, Change, Changeset, ChangesetId, EntryKind, Metadata, ObjectId, RepoPath,
    Timestamp, TreeId,
};

use crate::frame::Frame;

/// A page request.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Page {
    /// Maximum items.
    pub limit: u32,
    /// Resume token from a previous response.
    pub token: Option<String>,
}

// ---- objects --------------------------------------------------------------

/// `GetObjects` request.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct GetObjects {
    /// Repository.
    pub repo: String,
    /// Wanted ids (at most 1,024).
    pub ids: Vec<ObjectId>,
}

/// `GetObjects` response. Partial results are normal.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Objects {
    /// Found objects, as frames.
    pub found: Vec<(ObjectId, Frame)>,
    /// Not stored at this tier or above.
    pub missing: Vec<ObjectId>,
    /// Over the response size bound; ask again.
    pub deferred: Vec<ObjectId>,
}

/// `HasObjects` request.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct HasObjects {
    /// Repository.
    pub repo: String,
    /// Ids to check.
    pub ids: Vec<ObjectId>,
}

/// `PutObjects` request (blobs only).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct PutObjects {
    /// Repository.
    pub repo: String,
    /// Blob frames.
    pub blobs: Vec<Frame>,
}

// ---- changesets -----------------------------------------------------------

/// `CreateChangeset` request ("commit").
#[derive(Clone, Debug)]
#[non_exhaustive]
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
#[derive(Clone, Debug)]
#[non_exhaustive]
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
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct GetChangesets {
    /// Repository.
    pub repo: String,
    /// Ids.
    pub ids: Vec<ChangesetId>,
}

/// A changeset with its server record.
#[derive(Clone, Debug)]
#[non_exhaustive]
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
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
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
#[derive(Clone, Debug)]
#[non_exhaustive]
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
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ResolveRef {
    /// Repository.
    pub repo: String,
    /// Ref name.
    pub name: String,
}

/// `ListRefs` request.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ListRefs {
    /// Repository.
    pub repo: String,
    /// Name prefix.
    pub prefix: Option<String>,
    /// Paging.
    pub page: Page,
}

/// `UpdateRef` request (admin and tool refs).
#[derive(Clone, Debug)]
#[non_exhaustive]
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

// ---- snapshot reads -------------------------------------------------------

/// `Stat`, `ReadFile` request: a path in a changeset's snapshot.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct PathAt {
    /// Repository.
    pub repo: String,
    /// Changeset.
    pub at: ChangesetId,
    /// Path.
    pub path: RepoPath,
}

/// An entry in a snapshot.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct EntryView {
    /// Path.
    pub path: RepoPath,
    /// Kind.
    pub kind: EntryKind,
    /// Id.
    pub id: ObjectId,
}

/// `ReadFile` response.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct FileContent {
    /// Kind.
    pub kind: EntryKind,
    /// Blob id.
    pub id: BlobId,
    /// Bytes.
    pub data: Vec<u8>,
}

/// `ListTree` request.
#[derive(Clone, Debug)]
#[non_exhaustive]
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
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Entries {
    /// Entries.
    pub entries: Vec<EntryView>,
    /// Resume token.
    pub next: Option<String>,
}

// ---- history --------------------------------------------------------------

/// `Log` request.
#[derive(Clone, Debug)]
#[non_exhaustive]
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
#[derive(Clone, Debug)]
#[non_exhaustive]
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
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct History {
    /// Newest first.
    pub changesets: Vec<ChangesetSummary>,
    /// Resume token.
    pub next: Option<String>,
}

/// `IsAncestor` / `MergeBase` request.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Pair {
    /// Repository.
    pub repo: String,
    /// First changeset (the ancestor, for `IsAncestor`).
    pub a: ChangesetId,
    /// Second changeset.
    pub b: ChangesetId,
}

/// Options for unified patches in `Diff`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct PatchOptions {
    /// Context lines.
    pub context: u32,
    /// Cap per file, in bytes.
    pub max_bytes_per_file: u64,
}

/// `Diff` request.
#[derive(Clone, Debug)]
#[non_exhaustive]
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
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct FileDiff {
    /// The change.
    pub change: Change,
    /// Unified patch, when requested and text.
    pub patch: Option<String>,
    /// Whether the patch was truncated.
    pub truncated: bool,
}

/// `Diff` response.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct DiffResult {
    /// Files, sorted by path.
    pub files: Vec<FileDiff>,
    /// Resume token.
    pub next: Option<String>,
}

/// `Blame` request.
#[derive(Clone, Debug)]
#[non_exhaustive]
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
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BlameRange {
    /// First line (0-based).
    pub start: u32,
    /// One past the last line.
    pub end: u32,
    /// The changeset that introduced these lines.
    pub origin: ChangesetSummary,
}

/// `Blame` response.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
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
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Rebase {
    /// Repository.
    pub repo: String,
    /// The changeset (or stack head) to move.
    pub changeset: ChangesetId,
    /// The new base.
    pub onto: ChangesetId,
    /// Merge mode.
    pub merge: MergeMode,
}

/// `Revert` request.
#[derive(Clone, Debug)]
#[non_exhaustive]
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
#[derive(Clone, Debug)]
#[non_exhaustive]
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
}

/// `Land` success. The failures are `Conflict`, `Contended` and `Rejected`
/// in [`crate::ApiError`].
#[derive(Clone, Debug)]
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
#[derive(Clone, Debug)]
#[non_exhaustive]
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
#[derive(Clone, Debug)]
#[non_exhaustive]
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
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct JournalEntry {
    /// Position.
    pub lsn: u64,
    /// Entry kind, for example `RefMoved`.
    pub kind: String,
    /// The entry body, JSON-encoded.
    pub body: String,
}

/// A page of journal entries.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct JournalPage {
    /// Entries in LSN order.
    pub entries: Vec<JournalEntry>,
    /// Next LSN to ask for.
    pub next_lsn: u64,
}

// ---- workspaces -----------------------------------------------------------

/// A workspace (`docs/design.md` §3).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct WorkspaceView {
    /// Name.
    pub name: String,
    /// Owner subject.
    pub owner: String,
    /// Vogt session, if any.
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
#[derive(Clone, Debug)]
#[non_exhaustive]
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
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct WorkspaceRef {
    /// Repository.
    pub repo: String,
    /// Name.
    pub name: String,
}

/// `SetHead` request (CAS).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SetHead {
    /// The workspace.
    pub workspace: WorkspaceRef,
    /// Expected current head.
    pub expected: Option<ChangesetId>,
    /// New head.
    pub new: Option<ChangesetId>,
}

/// `Sync` response.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Synced {
    /// The workspace after the sync.
    pub workspace: WorkspaceView,
    /// Paths the client must update in its working copy.
    pub changed_paths: Vec<RepoPath>,
}

/// A stack operation.
#[derive(Clone, Debug)]
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
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ImportChangesets {
    /// Repository.
    pub repo: String,
    /// Changesets in topological order; roots allowed.
    pub changesets: Vec<Changeset>,
}

/// `Gc` request.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Gc {
    /// Repository.
    pub repo: String,
    /// Report only.
    pub dry_run: bool,
}

/// `Gc` report.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct GcReport {
    /// Changesets collected (or collectable, in a dry run).
    pub changesets: u64,
    /// Objects deleted (or doomed).
    pub objects: u64,
    /// Bytes reclaimed.
    pub bytes: u64,
}

/// `MintToken` request.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct MintToken {
    /// Subject.
    pub subject: String,
    /// Scopes; never broader than the caller's.
    pub scopes: Vec<String>,
    /// Lifetime in seconds.
    pub ttl_s: u64,
    /// Vogt session.
    pub session: Option<String>,
    /// Vogt work item.
    pub work_item: Option<String>,
}
