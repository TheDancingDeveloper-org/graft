//! Three-way text merge for line-level rebase (`docs/design.md` §7.8, work
//! item G3.4).

/// The outcome of a three-way merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Merge3 {
    /// Merged cleanly.
    Clean(Vec<u8>),
    /// Conflicting regions, as line ranges in `ours`.
    Conflicts(Vec<ConflictRegion>),
    /// One of the inputs is not text (binary, too large, not UTF-8).
    NotText,
}

/// A conflicting region.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictRegion {
    /// First line (0-based) in `ours`.
    pub start: usize,
    /// One past the last line in `ours`.
    pub end: usize,
}

/// Merges `ours` and `theirs` against `base`.
pub fn merge3(_base: &[u8], _ours: &[u8], _theirs: &[u8]) -> Merge3 {
    todo!("G3.4: diff3 over histogram diff")
}

/// Renders conflict markers for a client to write into the working copy.
pub fn render_conflict_markers(_base: &[u8], _ours: &[u8], _theirs: &[u8]) -> Vec<u8> {
    todo!("G3.4: conflict markers")
}
