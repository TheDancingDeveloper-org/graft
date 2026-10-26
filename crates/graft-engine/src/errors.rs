//! Mapping lower-layer failures onto the closed `ApiError` set
//! (`docs/design.md` §7.10). Nothing here carries file contents.

use graft_model::{ApplyError, ChangesetId, PathError, RepoPath, Rule, SourceError, Violation};
use graft_proto::ApiError;
use graft_store::StoreError;

/// The retry hint for a backend failure that has no hint of its own, ms.
pub(crate) const BACKEND_RETRY_MS: u64 = 1_000;

/// A storage failure.
///
/// Only a saturated store (`Busy`, the locked case) is retryable. Disk full,
/// corruption, a uniqueness clash, an invalid call and an unknown backend
/// fault are not: a client that retries them hides a durable failure.
/// `ApiError` has no internal variant, so those travel as `Rejected` with
/// policy `store` and a detail that is the kind name (`full`, `corrupt`,
/// `exists`, `invalid`, `backend`). Corruption is not `CorruptObject`: that
/// variant names one object id, and a store fault has none. The message
/// never carries the store's detail string, so file contents and tokens stay
/// out.
pub(crate) fn store(e: StoreError) -> ApiError {
    match e {
        StoreError::Busy(retry_after_ms) => ApiError::Unavailable { retry_after_ms },
        StoreError::Corrupt(_) => store_fault("corrupt"),
        StoreError::Full => store_fault("full"),
        StoreError::Exists => store_fault("exists"),
        StoreError::Invalid(_) => store_fault("invalid"),
        StoreError::Backend(_) => store_fault("backend"),
    }
}

fn store_fault(kind: &'static str) -> ApiError {
    ApiError::Rejected {
        policy: "store".to_owned(),
        detail: kind.to_owned(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_not_retryable(err: &ApiError) {
        assert!(!err.is_retryable(), "{err:?}");
        assert!(!matches!(err, ApiError::Unavailable { .. }), "{err:?}");
    }

    #[test]
    fn busy_is_unavailable_with_the_store_hint() {
        let err = store(StoreError::Busy(250));
        assert_eq!(
            err,
            ApiError::Unavailable {
                retry_after_ms: 250
            }
        );
        assert!(err.is_retryable());
    }

    #[test]
    fn full_is_not_retryable() {
        let err = store(StoreError::Full);
        assert_eq!(
            err,
            ApiError::Rejected {
                policy: "store".into(),
                detail: "full".into(),
            }
        );
        assert_not_retryable(&err);
    }

    #[test]
    fn corrupt_is_not_retryable_and_drops_the_detail() {
        let detail = "secret-token file contents at /var/lib/graft/objects";
        let err = store(StoreError::Corrupt(detail.to_owned()));
        assert_eq!(
            err,
            ApiError::Rejected {
                policy: "store".into(),
                detail: "corrupt".into(),
            }
        );
        assert_not_retryable(&err);
        let rendered = err.to_string();
        assert!(!rendered.contains(detail));
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("/var/"));
    }

    #[test]
    fn exists_is_not_retryable() {
        let err = store(StoreError::Exists);
        assert_eq!(
            err,
            ApiError::Rejected {
                policy: "store".into(),
                detail: "exists".into(),
            }
        );
        assert_not_retryable(&err);
    }

    #[test]
    fn invalid_is_not_retryable_and_drops_the_detail() {
        let detail = "blob bytes: token=abc /tmp/secret.txt";
        let err = store(StoreError::Invalid(detail.to_owned()));
        assert_eq!(
            err,
            ApiError::Rejected {
                policy: "store".into(),
                detail: "invalid".into(),
            }
        );
        assert_not_retryable(&err);
        assert!(!err.to_string().contains("token"));
        assert!(!err.to_string().contains("/tmp/"));
    }

    #[test]
    fn backend_is_not_retryable_and_drops_the_detail() {
        let detail = "io error reading /data/repo token=shh";
        let err = store(StoreError::Backend(detail.to_owned()));
        assert_eq!(
            err,
            ApiError::Rejected {
                policy: "store".into(),
                detail: "backend".into(),
            }
        );
        assert_not_retryable(&err);
        let rendered = err.to_string();
        assert!(!rendered.contains("token"));
        assert!(!rendered.contains("/data/"));
    }
}
