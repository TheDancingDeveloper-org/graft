//! Repository paths (`docs/design.md` §4.2).

use std::fmt;

/// A validated path relative to the repository root. Never the root itself.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoPath(String);

impl RepoPath {
    /// Validates `s` against the path rules.
    pub fn new(_s: &str) -> Result<Self, PathError> {
        todo!("G1.1: path rules")
    }

    /// Additionally checks the portability rules (case clashes are checked
    /// per directory by the engine; this checks reserved names).
    pub fn check_portable(&self) -> Result<(), PathError> {
        todo!("G1.1: portability rules")
    }

    /// The path as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The components, split on `/`.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// The parent directory, or `None` for a top-level entry.
    pub fn parent(&self) -> Option<RepoPath> {
        self.0.rsplit_once('/').map(|(p, _)| RepoPath(p.to_owned()))
    }

    /// Whether `self` equals `prefix` or lies beneath it.
    pub fn starts_with(&self, prefix: &RepoPath) -> bool {
        self.0 == prefix.0
            || (self.0.starts_with(&prefix.0)
                && self.0.as_bytes().get(prefix.0.len()) == Some(&b'/'))
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a path was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    /// Empty string (the root is not a path).
    Empty,
    /// Leading or trailing `/`, or an empty component.
    EmptyComponent,
    /// A `.` or `..` component.
    DotComponent,
    /// A NUL byte.
    Nul,
    /// A component over 255 bytes.
    ComponentTooLong,
    /// A path over 4,096 bytes.
    TooLong,
    /// Fails the portability rules (reserved name, case clash).
    NotPortable(String),
}
