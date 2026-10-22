//! The derivation framework: cached values keyed by `(kind, version, key)`,
//! and the walk that computes a missing value from the nearest computed
//! ones.
//!
//! A walk reads each changeset it has to compute (and its value's slot) once,
//! and stops on every path at the first changeset whose value is stored. It
//! never writes: it returns the values it computed as rows, for the caller
//! to commit (in the triggering write's transaction for eager kinds, or in a
//! cache-fill transaction of its own for lazy ones).

use std::collections::HashMap;

use graft_model::{Changeset, ChangesetId};
use graft_store::{MetaStore, ObjectStore, RepoId, StoreError, StoreResult, Write};

use crate::kinds::{SkipList, Skips};
use crate::{Index, ParentDerived, ValueKind, changeset_key, needed_parents};

/// A derived value and the rows that cache what was computed to get it
/// (empty when it was already stored).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Derived<V> {
    /// The value.
    pub value: V,
    /// `PutDerived` rows for every value the walk computed.
    pub rows: Vec<Write>,
}

/// The row that stores `value` for `key`.
pub fn row<K: ValueKind>(key: Vec<u8>, value: &K::Value) -> Write {
    Write::PutDerived {
        kind: K::NAME.to_owned(),
        version: K::VERSION,
        key,
        value: K::encode(value),
    }
}

/// One step of the depth-first walk.
enum Step {
    /// Find or compute this changeset's value.
    Visit(ChangesetId),
    /// Its needed parents have values; compute it.
    Finish(ChangesetId, Changeset),
}

impl<M: MetaStore, O: ObjectStore> Index<M, O> {
    /// The stored value for `key`, if any.
    pub async fn get<K: ValueKind>(
        &self,
        repo: RepoId,
        key: &[u8],
    ) -> StoreResult<Option<K::Value>> {
        match self.meta.derived(repo, K::NAME, K::VERSION, key).await? {
            None => Ok(None),
            Some(bytes) => K::decode(&bytes).map(Some).ok_or_else(|| {
                StoreError::Corrupt(format!("derived {}@{} value", K::NAME, K::VERSION))
            }),
        }
    }

    /// A stored changeset's content.
    async fn load(&self, repo: RepoId, id: ChangesetId) -> StoreResult<Changeset> {
        let (content, _) = self
            .meta
            .changeset(repo, id)
            .await?
            .ok_or_else(|| StoreError::Invalid(format!("no changeset {id}")))?;
        Changeset::decode(&content).map_err(|e| StoreError::Corrupt(format!("changeset {id}: {e}")))
    }

    /// `K`'s value for a stored changeset: read it, or walk back to the
    /// nearest stored values and compute forward.
    pub async fn derive<K: ParentDerived>(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> StoreResult<Derived<K::Value>> {
        let mut known = HashMap::new();
        let mut rows = Vec::new();
        self.walk::<K>(repo, id, &mut known, &mut rows).await?;
        let value = known.remove(&id).expect("the walk computed the target");
        Ok(Derived { value, rows })
    }

    /// `K`'s value for `cs` (whose id is `id`), which need not be stored yet.
    pub(crate) async fn derive_new<K: ParentDerived>(
        &self,
        repo: RepoId,
        id: ChangesetId,
        cs: &Changeset,
    ) -> StoreResult<Derived<K::Value>> {
        let mut known = HashMap::new();
        let mut rows = Vec::new();
        let parents = needed_parents(K::PARENTS, cs);
        for &p in parents {
            self.walk::<K>(repo, p, &mut known, &mut rows).await?;
        }
        let values: Vec<_> = parents.iter().map(|p| known[p].clone()).collect();
        let value = K::derive(cs, &values);
        rows.push(row::<K>(changeset_key(id), &value));
        Ok(Derived { value, rows })
    }

    /// Fills `known` with `target`'s value and those it needed, pushing a
    /// row for each one computed.
    async fn walk<K: ParentDerived>(
        &self,
        repo: RepoId,
        target: ChangesetId,
        known: &mut HashMap<ChangesetId, K::Value>,
        rows: &mut Vec<Write>,
    ) -> StoreResult<()> {
        let mut stack = vec![Step::Visit(target)];
        while let Some(step) = stack.pop() {
            match step {
                // In a DAG, everything above a visit on the stack finishes
                // before it pops, so a second visit finds the value.
                Step::Visit(id) => {
                    if known.contains_key(&id) {
                        continue;
                    }
                    if let Some(v) = self.get::<K>(repo, &changeset_key(id)).await? {
                        known.insert(id, v);
                        continue;
                    }
                    let cs = self.load(repo, id).await?;
                    let todo: Vec<_> = needed_parents(K::PARENTS, &cs)
                        .iter()
                        .filter(|p| !known.contains_key(p))
                        .map(|&p| Step::Visit(p))
                        .collect();
                    stack.push(Step::Finish(id, cs));
                    stack.extend(todo);
                }
                Step::Finish(id, cs) => {
                    let values: Vec<_> = needed_parents(K::PARENTS, &cs)
                        .iter()
                        .map(|p| known[p].clone())
                        .collect();
                    let value = K::derive(&cs, &values);
                    rows.push(row::<K>(changeset_key(id), &value));
                    known.insert(id, value);
                }
            }
        }
        Ok(())
    }

    /// The skip list of a stored changeset: read it, or walk first parents
    /// back to the nearest stored one and compute forward.
    pub async fn skip_list(&self, repo: RepoId, id: ChangesetId) -> StoreResult<Derived<Skips>> {
        let mut known = HashMap::new();
        let mut rows = Vec::new();
        let value = self.skips_walk(repo, id, &mut known, &mut rows).await?;
        Ok(Derived { value, rows })
    }

    /// The skip list for `cs` (whose id is `id`), which need not be stored.
    pub(crate) async fn skip_list_new(
        &self,
        repo: RepoId,
        id: ChangesetId,
        cs: &Changeset,
    ) -> StoreResult<Derived<Skips>> {
        let mut known = HashMap::new();
        let mut rows = Vec::new();
        let parent = match cs.first_parent() {
            None => None,
            Some(p) => Some((p, self.skips_walk(repo, p, &mut known, &mut rows).await?)),
        };
        let value = self.next_skips(repo, parent, &mut known).await?;
        rows.push(row::<SkipList>(changeset_key(id), &value));
        Ok(Derived { value, rows })
    }

    /// `target`'s skip list, computing the stretch of first parents back
    /// to the nearest stored one.
    async fn skips_walk(
        &self,
        repo: RepoId,
        target: ChangesetId,
        known: &mut HashMap<ChangesetId, Skips>,
        rows: &mut Vec<Write>,
    ) -> StoreResult<Skips> {
        // Changesets to compute, newest first, with their first parents.
        let mut chain = Vec::new();
        let mut base = None;
        let mut cur = Some(target);
        while let Some(id) = cur {
            if let Some(v) = self.get::<SkipList>(repo, &changeset_key(id)).await? {
                known.insert(id, v.clone());
                base = Some((id, v));
                break;
            }
            let parent = self.load(repo, id).await?.first_parent();
            chain.push(id);
            cur = parent;
        }
        let mut prev = base;
        for id in chain.into_iter().rev() {
            let value = self.next_skips(repo, prev.take(), known).await?;
            rows.push(row::<SkipList>(changeset_key(id), &value));
            known.insert(id, value.clone());
            prev = Some((id, value));
        }
        Ok(prev.expect("the walk reached the target").1)
    }

    /// The skip list of a changeset whose first parent (with its list) is
    /// `parent`. Entry `k` is entry `k - 1` of entry `k - 1`, so it reads
    /// O(log depth) lists of first-parent ancestors, which are stored
    /// whenever a descendant's list is.
    async fn next_skips(
        &self,
        repo: RepoId,
        parent: Option<(ChangesetId, Skips)>,
        known: &mut HashMap<ChangesetId, Skips>,
    ) -> StoreResult<Skips> {
        let Some((first, parent)) = parent else {
            return Ok(Skips {
                depth: 0,
                ancestors: Vec::new(),
            });
        };
        let mut ancestors = vec![first];
        if let Some(&second) = parent.ancestors.first() {
            ancestors.push(second);
            loop {
                let k = ancestors.len() - 1;
                let mid = ancestors[k];
                let next = match known.get(&mid) {
                    Some(v) => v.ancestors.get(k).copied(),
                    None => {
                        let v = self
                            .get::<SkipList>(repo, &changeset_key(mid))
                            .await?
                            .ok_or_else(|| {
                                StoreError::Corrupt(format!("no skip list for ancestor {mid}"))
                            })?;
                        let next = v.ancestors.get(k).copied();
                        known.insert(mid, v);
                        next
                    }
                };
                match next {
                    Some(id) => ancestors.push(id),
                    None => break,
                }
            }
        }
        Ok(Skips {
            depth: parent.depth + 1,
            ancestors,
        })
    }
}
