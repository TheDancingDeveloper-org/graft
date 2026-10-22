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
    ChangesetRecord, Lsn, MetaStore, MetaTxn, Precondition, RecordState, RefKind, RefRecord,
    TxnOutcome, WorkspaceRecord, Write,
};
pub use object::{ObjectStore, StoreStats};

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
    /// Any other backend failure, described.
    Backend(String),
}

/// Result alias for store calls.
pub type StoreResult<T> = Result<T, StoreError>;
