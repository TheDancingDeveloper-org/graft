//! The per-repository sweep lease: one sweeper per repository at a time.
//!
//! The lease is held in the process that runs the sweeps (the authority
//! server), and released when the pass ends, however it ends. A second
//! sweeper would still be safe, because every step is guarded by its own
//! preconditions; it would only be wasteful (design §8.6).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use graft_store::RepoId;

/// The leases of the repositories being swept. Clones share the set, so
/// every collector of one server should share one.
#[derive(Clone, Debug, Default)]
pub struct SweepLeases(Arc<Mutex<HashSet<RepoId>>>);

impl SweepLeases {
    /// No leases held.
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes `repo`'s lease, or `None` when another sweep holds it.
    pub fn try_acquire(&self, repo: RepoId) -> Option<SweepLease> {
        let mut held = self.0.lock().unwrap_or_else(|e| e.into_inner());
        held.insert(repo).then(|| SweepLease {
            leases: self.clone(),
            repo,
        })
    }

    /// Whether a sweep holds `repo`'s lease.
    pub fn is_held(&self, repo: RepoId) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&repo)
    }
}

/// A held lease; dropping it releases the repository.
#[derive(Debug)]
pub struct SweepLease {
    leases: SweepLeases,
    repo: RepoId,
}

impl Drop for SweepLease {
    fn drop(&mut self) {
        self.leases
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.repo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_holder_per_repository_until_released() {
        let leases = SweepLeases::new();
        let a = leases.try_acquire(RepoId(1)).expect("free");
        assert!(leases.try_acquire(RepoId(1)).is_none(), "held");
        let b = leases.clone().try_acquire(RepoId(2));
        assert!(b.is_some(), "another repository is free");
        drop(a);
        assert!(!leases.is_held(RepoId(1)));
        assert!(leases.try_acquire(RepoId(1)).is_some(), "released");
    }
}
