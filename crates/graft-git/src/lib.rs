//! git interop (`docs/design.md` §11). The only crate that knows git.
//!
//! Work items G1.8 (import and fidelity), G2.2 (export).

pub mod export;
pub mod import;
pub mod map;

/// A git object id (SHA-1, 20 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GitSha(pub [u8; 20]);

/// A git interop failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitError {
    /// Reading the git repository failed.
    Read(String),
    /// Paths that break graft's path rules (decision D11).
    BadPaths(Vec<String>),
    /// Commits whose export does not reproduce their sha.
    Fidelity(Vec<GitSha>),
    /// The graft API refused a write.
    Api(String),
    /// The mirror map failed.
    Map(String),
}
