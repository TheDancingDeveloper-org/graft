//! Ancestry over all parents: `IsAncestor` and `MergeBase` (`docs/design.md`
//! §4.9, §7.2).
//!
//! Both prune by generation (an ancestor's generation is strictly lower)
//! and by line position (on one line, position order is ancestry order), so
//! they visit only the changesets between the two arguments, not history.

use std::collections::{BinaryHeap, HashMap, HashSet};

use graft_model::ChangesetId;
use graft_store::{MetaStore, ObjectStore, RepoId, StoreResult};

use crate::Index;
use crate::kinds::Generation;

/// Paint flags for the merge-base walk.
const FROM_A: u8 = 1;
const FROM_B: u8 = 2;

impl<M: MetaStore, O: ObjectStore> Index<M, O> {
    /// A stored changeset's generation.
    async fn generation(&self, repo: RepoId, id: ChangesetId) -> StoreResult<u64> {
        Ok(self.derive::<Generation>(repo, id).await?.value)
    }

    /// Whether `a` is an ancestor of `b` through any parents (a changeset
    /// is its own ancestor).
    pub async fn is_ancestor(
        &self,
        repo: RepoId,
        a: ChangesetId,
        b: ChangesetId,
    ) -> StoreResult<bool> {
        if a == b {
            return Ok(true);
        }
        let ga = self.generation(repo, a).await?;
        if ga >= self.generation(repo, b).await? {
            return Ok(false);
        }
        let lines = self.lines(repo).await?;
        let mut a_lines = Vec::new();
        for line in &lines {
            if let Some(pos) = self.meta.line_pos(repo, line, a).await? {
                a_lines.push((line, pos));
            }
        }

        // Depth first from `b`, first parents first, never below `a`'s
        // generation.
        let mut seen = HashSet::from([b]);
        let mut stack = vec![b];
        'visit: while let Some(x) = stack.pop() {
            for &(line, pa) in &a_lines {
                if let Some(px) = self.meta.line_pos(repo, line, x).await? {
                    // On `a`'s line: `a` is below `x` (an ancestor), or
                    // above it (`x` is `a`'s ancestor, so not a descendant).
                    if pa <= px {
                        return Ok(true);
                    }
                    continue 'visit;
                }
            }
            let cs = self.load(repo, x).await?;
            for &p in cs.parents.iter().rev() {
                if p == a {
                    return Ok(true);
                }
                if seen.insert(p) && self.generation(repo, p).await? > ga {
                    stack.push(p);
                }
            }
        }
        Ok(false)
    }

    /// A best common ancestor of `a` and `b` (one that is not an ancestor
    /// of another common ancestor), or `None` when they share no history.
    /// With several best ones, the highest generation wins, then the
    /// highest id.
    pub async fn merge_base(
        &self,
        repo: RepoId,
        a: ChangesetId,
        b: ChangesetId,
    ) -> StoreResult<Option<ChangesetId>> {
        if a == b {
            return Ok(Some(a));
        }
        // Both on one line: the lower one is the other's ancestor.
        for line in self.lines(repo).await? {
            let pa = self.meta.line_pos(repo, &line, a).await?;
            if let Some(pa) = pa
                && let Some(pb) = self.meta.line_pos(repo, &line, b).await?
            {
                return Ok(Some(if pa <= pb { a } else { b }));
            }
        }

        // Paint down from both in generation order, so every changeset is
        // taken after all its descendants in the walk and its flags are
        // final. The first one painted by both is a best common ancestor:
        // a common descendant of it would have been taken (and painted by
        // both) before it.
        let mut flags: HashMap<ChangesetId, u8> = HashMap::from([(a, FROM_A), (b, FROM_B)]);
        let mut queue = BinaryHeap::from([
            (self.generation(repo, a).await?, a),
            (self.generation(repo, b).await?, b),
        ]);
        let mut done = HashSet::new();
        while let Some((_, x)) = queue.pop() {
            if !done.insert(x) {
                continue;
            }
            let f = flags[&x];
            if f == FROM_A | FROM_B {
                return Ok(Some(x));
            }
            for p in self.load(repo, x).await?.parents {
                let pf = flags.entry(p).or_default();
                if *pf & f == f {
                    continue;
                }
                *pf |= f;
                queue.push((self.generation(repo, p).await?, p));
            }
        }
        Ok(None)
    }
}
