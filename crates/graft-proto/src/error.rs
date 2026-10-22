//! The closed set of API errors (`docs/design.md` §7.10). An error never
//! carries file contents or tokens.

use graft_model::{BlobId, ChangesetId, ObjectId};

/// A path that conflicted in a rebase, sync or land, by id only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictPath {
    /// The conflicting path.
    pub path: String,
    /// The entry in the base, if any.
    pub base: Option<ObjectId>,
    /// The entry on the changeset's side, if any.
    pub ours: Option<ObjectId>,
    /// The entry on the target's side, if any.
    pub theirs: Option<ObjectId>,
}

/// Every error an API operation can return.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApiError {
    /// A named changeset, ref, workspace or path does not exist.
    NotFound {
        /// What was not found.
        what: String,
    },
    /// The changeset existed but has been collected.
    Collected {
        /// Its id.
        id: ChangesetId,
    },
    /// A path broke the path rules.
    InvalidPath {
        /// The path.
        path: String,
        /// Which rule.
        reason: String,
    },
    /// A changeset request broke a validity rule.
    InvalidChange {
        /// The path.
        path: String,
        /// Which rule.
        rule: String,
    },
    /// A parent is not in this repository.
    UnknownParent {
        /// The parent.
        id: ChangesetId,
    },
    /// Referenced blobs are not stored (or were collected); upload them.
    MissingObjects {
        /// The missing ids.
        ids: Vec<ObjectId>,
    },
    /// A compare-and-swap lost.
    RefConflict {
        /// The ref.
        name: String,
        /// What it holds now.
        actual: Option<ChangesetId>,
    },
    /// A rebase, revert, sync or land found conflicting paths.
    Conflict {
        /// The conflicts.
        paths: Vec<ConflictPath>,
        /// The changeset it was rebased onto.
        onto: ChangesetId,
    },
    /// Land ran out of attempts under contention; retryable.
    Contended {
        /// The line.
        line: String,
        /// Its tip at the last attempt.
        tip: ChangesetId,
    },
    /// A land policy refused.
    Rejected {
        /// The policy.
        policy: String,
        /// Why, for people and agents.
        detail: String,
    },
    /// The caller lacks a scope.
    PermissionDenied {
        /// The scope that was needed.
        scope: String,
        /// The path, for path-scoped checks.
        path: Option<String>,
    },
    /// The workspace lease lapsed.
    LeaseExpired {
        /// The workspace.
        workspace: String,
    },
    /// A batch, blob, page or change-count limit was exceeded.
    LimitExceeded {
        /// The limit's name.
        limit: String,
        /// Its value.
        max: u64,
    },
    /// Stored or received bytes do not hash to their id.
    CorruptObject {
        /// The id.
        id: ObjectId,
    },
    /// The blob was obliterated by an admin.
    Obliterated {
        /// The blob.
        id: BlobId,
    },
    /// Shedding load or upstream unreachable; retry after the hint.
    Unavailable {
        /// Suggested wait in milliseconds.
        retry_after_ms: u64,
    },
    /// No valid credentials.
    Unauthenticated,
    /// Client and server protocol versions do not bind.
    IncompatibleProtocol {
        /// The server's version, as `major.minor`.
        server: String,
    },
}

impl ApiError {
    /// Whether a client may retry the same request unchanged.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            ApiError::Contended { .. } | ApiError::Unavailable { .. }
        )
    }
}

/// Result alias for API calls.
pub type ApiResult<T> = Result<T, ApiError>;
