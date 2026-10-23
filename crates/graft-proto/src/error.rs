//! The closed set of API errors (`docs/design.md` §7.10). An error never
//! carries file contents or tokens.

use std::fmt;

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
    /// The error an operation that is designed but not built yet returns.
    /// It travels as `Rejected` with policy `unimplemented`, so it needs no
    /// new wire variant.
    pub fn unimplemented(op: &str) -> ApiError {
        ApiError::Rejected {
            policy: "unimplemented".to_owned(),
            detail: format!("{op} is not built yet"),
        }
    }

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

impl ApiError {
    /// The gRPC status code this error travels under. The code is for
    /// generic gRPC tooling; graft clients read the typed error from the
    /// status details.
    pub fn grpc_code(&self) -> tonic::Code {
        use tonic::Code;
        match self {
            ApiError::NotFound { .. } | ApiError::Collected { .. } => Code::NotFound,
            ApiError::Obliterated { .. } => Code::NotFound,
            ApiError::InvalidPath { .. } | ApiError::InvalidChange { .. } => Code::InvalidArgument,
            ApiError::UnknownParent { .. }
            | ApiError::MissingObjects { .. }
            | ApiError::Rejected { .. }
            | ApiError::LeaseExpired { .. }
            | ApiError::IncompatibleProtocol { .. } => Code::FailedPrecondition,
            ApiError::RefConflict { .. }
            | ApiError::Conflict { .. }
            | ApiError::Contended { .. } => Code::Aborted,
            ApiError::PermissionDenied { .. } => Code::PermissionDenied,
            ApiError::LimitExceeded { .. } => Code::ResourceExhausted,
            ApiError::CorruptObject { .. } => Code::DataLoss,
            ApiError::Unavailable { .. } => Code::Unavailable,
            ApiError::Unauthenticated => Code::Unauthenticated,
        }
    }

    /// Reads the typed error from a status produced by `Status::from`.
    /// Returns `None` for a status that carries no graft error (a transport
    /// failure, or a peer that is not graft).
    pub fn from_status(status: &tonic::Status) -> Option<ApiError> {
        use prost::Message;
        let wire = crate::pb::ApiError::decode(status.details()).ok()?;
        ApiError::try_from(wire).ok()
    }
}

impl From<ApiError> for tonic::Status {
    fn from(e: ApiError) -> Self {
        use prost::Message;
        let code = e.grpc_code();
        let message = e.to_string();
        let details = crate::pb::ApiError::from(e).encode_to_vec();
        tonic::Status::with_details(code, message, details.into())
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::NotFound { what } => write!(f, "not found: {what}"),
            ApiError::Collected { id } => write!(f, "changeset {id} was collected"),
            ApiError::InvalidPath { path, reason } => write!(f, "invalid path {path:?}: {reason}"),
            ApiError::InvalidChange { path, rule } => {
                write!(f, "invalid change at {path:?}: {rule}")
            }
            ApiError::UnknownParent { id } => write!(f, "unknown parent {id}"),
            ApiError::MissingObjects { ids } => write!(f, "{} objects are missing", ids.len()),
            ApiError::RefConflict { name, .. } => write!(f, "ref {name} moved"),
            ApiError::Conflict { paths, onto } => {
                write!(f, "{} paths conflict onto {onto}", paths.len())
            }
            ApiError::Contended { line, .. } => write!(f, "line {line} is contended"),
            ApiError::Rejected { policy, detail } => write!(f, "rejected by {policy}: {detail}"),
            ApiError::PermissionDenied { scope, path: None } => {
                write!(f, "permission denied: needs {scope}")
            }
            ApiError::PermissionDenied {
                scope,
                path: Some(p),
            } => write!(f, "permission denied: needs {scope} on {p:?}"),
            ApiError::LeaseExpired { workspace } => {
                write!(f, "workspace {workspace} lease expired")
            }
            ApiError::LimitExceeded { limit, max } => write!(f, "limit {limit} ({max}) exceeded"),
            ApiError::CorruptObject { id } => write!(f, "object {} is corrupt", id.to_hex()),
            ApiError::Obliterated { id } => write!(f, "blob {id} was obliterated"),
            ApiError::Unavailable { retry_after_ms } => {
                write!(f, "unavailable; retry after {retry_after_ms} ms")
            }
            ApiError::Unauthenticated => f.write_str("unauthenticated"),
            ApiError::IncompatibleProtocol { server } => {
                write!(f, "incompatible protocol; server speaks {server}")
            }
        }
    }
}

impl std::error::Error for ApiError {}
