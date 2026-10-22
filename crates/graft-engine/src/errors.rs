//! Mapping lower-layer failures onto the closed `ApiError` set
//! (`docs/design.md` §7.10). Nothing here carries file contents.

use graft_model::{ApplyError, ChangesetId, PathError, RepoPath, Rule, SourceError, Violation};
use graft_proto::ApiError;
use graft_store::StoreError;

/// The retry hint for a backend failure that has no hint of its own, ms.
pub(crate) const BACKEND_RETRY_MS: u64 = 1_000;

/// A storage failure. `ApiError` has no internal-error variant, so a backend
/// failure is reported as `Unavailable`: the request did not take effect and
/// may be retried.
pub(crate) fn store(e: StoreError) -> ApiError {
    let retry_after_ms = match e {
        StoreError::Busy(ms) => ms,
        StoreError::Full
        | StoreError::Corrupt(_)
        | StoreError::Exists
        | StoreError::Invalid(_)
        | StoreError::Backend(_) => BACKEND_RETRY_MS,
    };
    ApiError::Unavailable { retry_after_ms }
}

/// A failure reading a tree.
pub(crate) fn source(e: SourceError) -> ApiError {
    match e {
        SourceError::Missing(id) => ApiError::MissingObjects { ids: vec![id.0] },
        SourceError::Corrupt(id) | SourceError::Malformed(id) => {
            ApiError::CorruptObject { id: id.0 }
        }
        SourceError::Backend(_) => ApiError::Unavailable {
            retry_after_ms: BACKEND_RETRY_MS,
        },
    }
}

/// A broken validity rule.
pub(crate) fn violation(v: Violation) -> ApiError {
    let path = v.path.as_ref().map_or_else(String::new, |p| p.to_string());
    match v.rule {
        Rule::Path(e) => ApiError::InvalidPath {
            path,
            reason: e.reason().to_owned(),
        },
        rule => ApiError::InvalidChange {
            path,
            rule: rule.as_str().to_owned(),
        },
    }
}

/// A failed `apply_changes`.
pub(crate) fn apply(e: ApplyError) -> ApiError {
    match e {
        ApplyError::Invalid(v) => violation(v),
        ApplyError::Source(e) => source(e),
    }
}

/// A malformed path or path-shaped token.
pub(crate) fn path(text: &str, e: PathError) -> ApiError {
    ApiError::InvalidPath {
        path: text.to_owned(),
        reason: e.reason().to_owned(),
    }
}

/// `InvalidChange` at `path` (empty when the rule is about the request).
pub(crate) fn invalid(path: Option<&RepoPath>, rule: &str) -> ApiError {
    ApiError::InvalidChange {
        path: path.map_or_else(String::new, |p| p.to_string()),
        rule: rule.to_owned(),
    }
}

/// `LimitExceeded`.
pub(crate) fn limit(name: &str, max: u64) -> ApiError {
    ApiError::LimitExceeded {
        limit: name.to_owned(),
        max,
    }
}

/// `NotFound`.
pub(crate) fn not_found(what: impl Into<String>) -> ApiError {
    ApiError::NotFound { what: what.into() }
}

/// A stored changeset whose bytes do not decode.
pub(crate) fn corrupt_changeset(id: ChangesetId) -> ApiError {
    ApiError::CorruptObject { id: id.0 }
}
