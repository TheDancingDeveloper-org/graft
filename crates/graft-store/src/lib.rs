//! Storage ports (`docs/design.md` §6.1).
//!
//! The engine never names a backend. [`ObjectStore`] holds content-addressed
//! frames; [`MetaStore`] holds everything else, with transactions scoped to
//! one logical write plus its journal entry ([`MetaTxn`]). With the `testkit`
//! feature this crate also provides the in-memory store and the conformance
//! suite every implementation must pass (work item G1.2).

pub mod meta;
pub mod object;

#[cfg(feature = "testkit")]
pub mod conformance;
#[cfg(feature = "testkit")]
pub mod memory;

pub use meta::{
    ChangesetRecord, DoomMark, Lsn, MetaStore, MetaTxn, Precondition, Protection, RecordState,
    RefKind, RefRecord, TimedRow, TxnOutcome, WorkspaceRecord, Write,
};
pub use object::{ObjectStore, StoreStats};

/// The largest page any list call returns. Every list takes a `limit`, and
/// implementations clamp it to `1..=MAX_PAGE` (see [`page_limit`]).
pub const MAX_PAGE: u32 = 1000;

/// The number of rows a list call returns for a requested `limit`: the limit
/// clamped to `1..=MAX_PAGE`. Every implementation uses it, so the bound is
/// the same for every store.
pub fn page_limit(limit: u32) -> usize {
    limit.clamp(1, MAX_PAGE) as usize
}

/// A repository's internal id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoId(pub u32);

/// A storage failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    /// The backend is saturated; retry after the hint (ms).
    Busy(u64),
    /// Not enough free space to accept writes.
    Full,
    /// Stored bytes failed verification.
    Corrupt(String),
    /// A name that must be unique is taken (for example a repository).
    Exists,
    /// The caller broke a port rule, such as a write to an unknown repository
    /// or a ref version that is not the next one. Nothing was written.
    Invalid(String),
    /// Any other backend failure, described.
    Backend(String),
}

/// Result alias for store calls.
pub type StoreResult<T> = Result<T, StoreError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_limit_clamps_to_one_through_the_maximum() {
        assert_eq!(page_limit(0), 1);
        assert_eq!(page_limit(1), 1);
        assert_eq!(page_limit(MAX_PAGE), MAX_PAGE as usize);
        assert_eq!(page_limit(u32::MAX), MAX_PAGE as usize);
    }
}
