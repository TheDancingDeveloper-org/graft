//! Decoded changesets, kept in memory (WI-2341).
//!
//! A changeset never changes: its id is the hash of its content. A history
//! page reads one changeset per result, and each read is a metadata-store
//! round trip and a decode, so the index keeps the changesets it has read
//! and answers the next read of the same id from memory. The bound is on
//! the changes held, not the changesets, so a few huge imports cannot hold
//! memory a page of small ones would not.

use std::collections::HashMap;
use std::mem;
use std::sync::{Mutex, PoisonError};

use graft_model::{Changeset, ChangesetId};
use graft_store::RepoId;

/// The most changes (plus one per changeset) the cache holds.
pub(crate) const CACHE_WEIGHT: usize = 1 << 18;

type Key = (RepoId, ChangesetId);

/// Two generations: reads fill the young one, and when it is full it
/// becomes the old one, dropping the old. A hit in the old generation moves
/// to the young. At most twice the generation weight is held.
#[derive(Debug)]
pub(crate) struct ChangesetCache {
    generation: usize,
    inner: Mutex<Generations>,
}

#[derive(Debug, Default)]
struct Generations {
    young: HashMap<Key, Changeset>,
    young_weight: usize,
    old: HashMap<Key, Changeset>,
}

fn weight(cs: &Changeset) -> usize {
    cs.changes.len() + 1
}

impl ChangesetCache {
    /// A cache holding about `max_weight` changes.
    pub(crate) fn new(max_weight: usize) -> Self {
        Self {
            generation: (max_weight / 2).max(1),
            inner: Mutex::new(Generations::default()),
        }
    }

    pub(crate) fn get(&self, repo: RepoId, id: ChangesetId) -> Option<Changeset> {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(cs) = g.young.get(&(repo, id)) {
            return Some(cs.clone());
        }
        let cs = g.old.remove(&(repo, id))?;
        let out = cs.clone();
        self.insert_locked(&mut g, (repo, id), cs);
        Some(out)
    }

    pub(crate) fn insert(&self, repo: RepoId, id: ChangesetId, cs: &Changeset) {
        if weight(cs) > self.generation {
            return;
        }
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if !g.young.contains_key(&(repo, id)) {
            self.insert_locked(&mut g, (repo, id), cs.clone());
        }
    }

    fn insert_locked(&self, g: &mut Generations, key: Key, cs: Changeset) {
        let w = weight(&cs);
        if g.young_weight + w > self.generation {
            g.old = mem::take(&mut g.young);
            g.young_weight = 0;
        }
        g.young_weight += w;
        g.young.insert(key, cs);
    }

    #[cfg(test)]
    fn held(&self) -> usize {
        let g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.young.values().chain(g.old.values()).map(weight).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_model::{Author, AuthorKind, Change, ObjectId, RepoPath, Timestamp, TreeId};

    fn cs(n: u64, changes: usize) -> (ChangesetId, Changeset) {
        let cs = Changeset {
            parents: Vec::new(),
            root: TreeId(ObjectId([0; 32])),
            changes: (0..changes)
                .map(|i| Change::Delete {
                    path: RepoPath::new(&format!("f{i:04}")).unwrap(),
                })
                .collect(),
            author: Author {
                name: "t".into(),
                email: "t@example.com".into(),
                kind: AuthorKind::Agent,
            },
            authored_at: Timestamp {
                ms: 0,
                offset_min: 0,
            },
            message: format!("c{n}"),
            metadata: Default::default(),
        };
        (cs.id(), cs)
    }

    #[test]
    fn hits_return_what_was_stored_and_the_bound_holds() {
        let cache = ChangesetCache::new(100);
        let repo = RepoId(1);
        let all: Vec<_> = (0..40).map(|n| cs(n, 4)).collect();
        for (id, c) in &all {
            assert_eq!(cache.get(repo, *id), None);
            cache.insert(repo, *id, c);
            assert_eq!(cache.get(repo, *id).as_ref(), Some(c));
            assert!(cache.held() <= 100, "{} held", cache.held());
        }
        // The newest are kept; another repository's ids are separate.
        let (id, c) = &all[39];
        assert_eq!(cache.get(repo, *id).as_ref(), Some(c));
        assert_eq!(cache.get(RepoId(2), *id), None);
    }

    #[test]
    fn a_changeset_over_a_generation_is_not_kept() {
        let cache = ChangesetCache::new(10);
        let (id, c) = cs(1, 50);
        cache.insert(RepoId(1), id, &c);
        assert_eq!(cache.get(RepoId(1), id), None);
    }
}
