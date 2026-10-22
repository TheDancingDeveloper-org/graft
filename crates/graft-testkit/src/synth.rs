//! Seeded synthetic repositories (work item G1.2).

/// Shape of a synthetic repository.
#[derive(Clone, Debug)]
pub struct SynthSpec {
    /// Files in the final snapshot.
    pub files: usize,
    /// Maximum directory depth.
    pub depth: usize,
    /// Changesets of history.
    pub history: usize,
    /// Average file size, bytes.
    pub file_bytes: usize,
    /// Seed.
    pub seed: u64,
}

/// A generated repository: snapshots as path → bytes, oldest first.
#[derive(Clone, Debug, Default)]
pub struct SynthRepo {
    /// Snapshots.
    pub snapshots: Vec<std::collections::BTreeMap<String, Vec<u8>>>,
}

/// Generates a repository deterministically from `spec`.
pub fn generate(_spec: &SynthSpec) -> SynthRepo {
    todo!("G1.2")
}
