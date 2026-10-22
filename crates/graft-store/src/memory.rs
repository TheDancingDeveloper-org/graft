//! In-memory `ObjectStore` and `MetaStore`, for tests only (work item G1.2).
//!
//! Every call takes one mutex, so a transaction is atomic by construction:
//! it is validated in full before the first write is applied. Futures are
//! ready on return. Clones share one store, so a single value can serve as
//! both ports, as the conformance factory needs.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::{Future, ready};
use std::sync::{Arc, Mutex};

use graft_model::{ChangesetId, ObjectId, RepoPath};

use crate::meta::{
    ChangesetRecord, Lsn, MetaStore, MetaTxn, Precondition, RecordState, RefRecord, TxnOutcome,
    WorkspaceRecord, Write,
};
use crate::object::{ObjectStore, StoreStats};
use crate::{RepoId, StoreError, StoreResult, page_limit};

/// One repository's metadata.
#[derive(Debug, Default)]
struct Repo {
    changesets: BTreeMap<ChangesetId, (Vec<u8>, ChangesetRecord)>,
    refs: BTreeMap<String, RefRecord>,
    workspaces: BTreeMap<String, WorkspaceRecord>,
    derived: BTreeMap<(String, u32, Vec<u8>), Vec<u8>>,
    line_pos: BTreeMap<(String, ChangesetId), u64>,
    line_paths: BTreeMap<(String, RepoPath), BTreeSet<(u64, ChangesetId)>>,
    holders: BTreeSet<(ObjectId, String)>,
    lands: BTreeMap<(ChangesetId, String), ChangesetId>,
    /// Entry `i` has LSN `i + 1`.
    journal: Vec<(Lsn, String, Vec<u8>)>,
}

/// The whole store.
#[derive(Debug, Default)]
struct State {
    names: BTreeMap<String, RepoId>,
    repos: BTreeMap<RepoId, Repo>,
    next_repo: u32,
    /// Frames are keyed by repository too: objects are per repository.
    objects: BTreeMap<(RepoId, ObjectId), Vec<u8>>,
}

impl State {
    fn repo(&self, id: RepoId) -> StoreResult<&Repo> {
        self.repos
            .get(&id)
            .ok_or_else(|| StoreError::Invalid(format!("unknown repository {}", id.0)))
    }

    fn repo_mut(&mut self, id: RepoId) -> StoreResult<&mut Repo> {
        self.repos
            .get_mut(&id)
            .ok_or_else(|| StoreError::Invalid(format!("unknown repository {}", id.0)))
    }
}

impl Repo {
    /// Whether a precondition holds right now.
    fn holds(&self, pre: &Precondition) -> bool {
        match pre {
            Precondition::RefIs { name, expected } => {
                let current = self.refs.get(name).map(|r| (r.target, r.version));
                current == *expected
            }
            Precondition::WorkspaceHeadIs { name, head } => {
                self.workspaces.get(name).is_some_and(|ws| ws.head == *head)
            }
            Precondition::ChangesetAbsent(id) => !self.changesets.contains_key(id),
            Precondition::WorkspaceIs { name, expected } => {
                self.workspaces.get(name) == expected.as_ref()
            }
            Precondition::RecordStateIs { id, state } => self
                .changesets
                .get(id)
                .is_some_and(|(_, record)| record.state == *state),
        }
    }
}

/// Checks a transaction's writes against the state it would apply to, so
/// that applying them cannot fail.
fn validate(repo: &Repo, txn: &MetaTxn) -> StoreResult<()> {
    // Versions as the writes so far would leave them.
    let mut versions: HashMap<&str, u64> = HashMap::new();
    let mut created: BTreeSet<ChangesetId> = BTreeSet::new();
    for write in &txn.writes {
        match write {
            Write::PutChangeset { id, .. } => {
                created.insert(*id);
            }
            Write::SetRecord { id, .. } => {
                if !created.contains(id) && !repo.changesets.contains_key(id) {
                    return Err(StoreError::Invalid(format!("no changeset {id} to update")));
                }
            }
            Write::SetRef(record) => {
                let guarded = txn
                    .preconditions
                    .iter()
                    .any(|p| matches!(p, Precondition::RefIs { name, .. } if *name == record.name));
                if !guarded {
                    return Err(StoreError::Invalid(format!(
                        "ref {} is set without a RefIs precondition",
                        record.name
                    )));
                }
                let current = versions
                    .get(record.name.as_str())
                    .copied()
                    .unwrap_or_else(|| repo.refs.get(&record.name).map_or(0, |r| r.version));
                if record.version != current + 1 {
                    return Err(StoreError::Invalid(format!(
                        "ref {} version {} does not follow {current}",
                        record.name, record.version
                    )));
                }
                versions.insert(record.name.as_str(), record.version);
            }
            _ => {}
        }
    }
    Ok(())
}

/// Commits one transaction. Preconditions are checked first, in order; then
/// the writes are validated; then they are applied.
fn commit_txn(st: &mut State, id: RepoId, txn: MetaTxn) -> StoreResult<TxnOutcome> {
    let repo = st.repo_mut(id)?;
    if let Some(index) = txn.preconditions.iter().position(|p| !repo.holds(p)) {
        return Ok(TxnOutcome::PreconditionFailed(index));
    }
    validate(repo, &txn)?;

    let has_journal = txn
        .writes
        .iter()
        .any(|w| matches!(w, Write::Journal { .. }));
    // The first journal entry of this transaction: records take its LSN.
    let first_lsn = repo.journal.len() as u64 + 1;
    let mut lsns = Vec::new();
    for write in txn.writes {
        match write {
            Write::PutChangeset {
                id,
                content,
                mut record,
            } => {
                if has_journal {
                    record.lsn = first_lsn;
                }
                repo.changesets.insert(id, (content, record));
            }
            Write::SetRecord { id, mut record } => {
                if has_journal {
                    record.lsn = first_lsn;
                }
                // Validated above: the changeset exists by now.
                if let Some(slot) = repo.changesets.get_mut(&id) {
                    slot.1 = record;
                }
            }
            Write::SetRef(record) => {
                repo.refs.insert(record.name.clone(), record);
            }
            Write::PutWorkspace(ws) => {
                repo.workspaces.insert(ws.name.clone(), ws);
            }
            Write::DeleteWorkspace(name) => {
                repo.workspaces.remove(&name);
            }
            Write::PutDerived {
                kind,
                version,
                key,
                value,
            } => {
                repo.derived.insert((kind, version, key), value);
            }
            Write::PutLinePos {
                line,
                changeset,
                pos,
            } => {
                repo.line_pos.insert((line, changeset), pos);
            }
            Write::PutLinePath {
                line,
                path,
                pos,
                changeset,
            } => {
                repo.line_paths
                    .entry((line, path))
                    .or_default()
                    .insert((pos, changeset));
            }
            Write::AddHolder { object, holder } => {
                repo.holders.insert((object, holder));
            }
            Write::RemoveHolder { object, holder } => {
                repo.holders.remove(&(object, holder));
            }
            Write::PutLand {
                draft,
                line,
                landed,
            } => {
                repo.lands.entry((draft, line)).or_insert(landed);
            }
            Write::Journal { kind, body } => {
                let lsn = repo.journal.len() as u64 + 1;
                repo.journal.push((lsn, kind, body));
                lsns.push(lsn);
            }
        }
    }
    Ok(TxnOutcome::Committed(lsns))
}

/// An in-memory store implementing both ports.
#[derive(Clone, Debug, Default)]
pub struct MemoryStore {
    state: Arc<Mutex<State>>,
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn with<T>(&self, f: impl FnOnce(&mut State) -> StoreResult<T>) -> StoreResult<T> {
        let mut guard = self.state.lock().expect("memory store mutex poisoned");
        f(&mut guard)
    }
}

impl ObjectStore for MemoryStore {
    fn put(
        &self,
        repo: RepoId,
        id: ObjectId,
        frame: Vec<u8>,
    ) -> impl Future<Output = StoreResult<()>> + Send {
        ready(self.with(|st| {
            st.repo(repo)?;
            st.objects.entry((repo, id)).or_insert(frame);
            Ok(())
        }))
    }

    fn get(
        &self,
        repo: RepoId,
        id: ObjectId,
    ) -> impl Future<Output = StoreResult<Option<Vec<u8>>>> + Send {
        ready(self.with(|st| {
            st.repo(repo)?;
            Ok(st.objects.get(&(repo, id)).cloned())
        }))
    }

    fn has(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
    ) -> impl Future<Output = StoreResult<Vec<ObjectId>>> + Send {
        ready(self.with(|st| {
            st.repo(repo)?;
            Ok(ids
                .iter()
                .copied()
                .filter(|id| st.objects.contains_key(&(repo, *id)))
                .collect())
        }))
    }

    fn delete(&self, repo: RepoId, id: ObjectId) -> impl Future<Output = StoreResult<()>> + Send {
        ready(self.with(|st| {
            st.repo(repo)?;
            st.objects.remove(&(repo, id));
            Ok(())
        }))
    }

    fn stats(&self) -> impl Future<Output = StoreResult<StoreStats>> + Send {
        ready(self.with(|st| {
            let bytes = st.objects.values().map(|f| f.len() as u64).sum();
            Ok(StoreStats {
                objects: st.objects.len() as u64,
                bytes,
                // Memory has no volume to run out of.
                free_bytes: u64::MAX,
            })
        }))
    }
}

impl MetaStore for MemoryStore {
    fn create_repo(&self, name: &str) -> impl Future<Output = StoreResult<RepoId>> + Send {
        ready(self.with(|st| {
            if st.names.contains_key(name) {
                return Err(StoreError::Exists);
            }
            st.next_repo += 1;
            let id = RepoId(st.next_repo);
            st.names.insert(name.to_owned(), id);
            st.repos.insert(id, Repo::default());
            Ok(id)
        }))
    }

    fn repo(&self, name: &str) -> impl Future<Output = StoreResult<Option<RepoId>>> + Send {
        ready(self.with(|st| Ok(st.names.get(name).copied())))
    }

    fn commit(
        &self,
        repo: RepoId,
        txn: MetaTxn,
    ) -> impl Future<Output = StoreResult<TxnOutcome>> + Send {
        ready(self.with(|st| commit_txn(st, repo, txn)))
    }

    fn changeset(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> impl Future<Output = StoreResult<Option<(Vec<u8>, ChangesetRecord)>>> + Send {
        ready(self.with(|st| Ok(st.repo(repo)?.changesets.get(&id).cloned())))
    }

    fn get_ref(
        &self,
        repo: RepoId,
        name: &str,
    ) -> impl Future<Output = StoreResult<Option<RefRecord>>> + Send {
        ready(self.with(|st| Ok(st.repo(repo)?.refs.get(name).cloned())))
    }

    fn list_refs(
        &self,
        repo: RepoId,
        prefix: &str,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<RefRecord>>> + Send {
        ready(self.with(|st| {
            Ok(st
                .repo(repo)?
                .refs
                .iter()
                .filter(|(name, _)| name.starts_with(prefix))
                .take(page_limit(limit))
                .map(|(_, r)| r.clone())
                .collect())
        }))
    }

    fn workspace(
        &self,
        repo: RepoId,
        name: &str,
    ) -> impl Future<Output = StoreResult<Option<WorkspaceRecord>>> + Send {
        ready(self.with(|st| Ok(st.repo(repo)?.workspaces.get(name).cloned())))
    }

    fn expired_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send {
        ready(self.with(|st| {
            Ok(st
                .repo(repo)?
                .workspaces
                .values()
                .filter(|ws| ws.lease_until_ms < before_ms)
                .take(page_limit(limit))
                .cloned()
                .collect())
        }))
    }

    fn list_workspaces(
        &self,
        repo: RepoId,
        owner: &str,
        after: &str,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send {
        ready(self.with(|st| {
            use std::ops::Bound;
            Ok(st
                .repo(repo)?
                .workspaces
                .range::<str, _>((Bound::Excluded(after), Bound::Unbounded))
                .map(|(_, ws)| ws)
                .filter(|ws| ws.owner == owner)
                .take(page_limit(limit))
                .cloned()
                .collect())
        }))
    }

    fn count_records(
        &self,
        repo: RepoId,
        state: RecordState,
    ) -> impl Future<Output = StoreResult<u64>> + Send {
        ready(self.with(|st| {
            Ok(st
                .repo(repo)?
                .changesets
                .values()
                .filter(|(_, record)| record.state == state)
                .count() as u64)
        }))
    }

    fn count_workspaces(
        &self,
        repo: RepoId,
        before_ms: Option<i64>,
    ) -> impl Future<Output = StoreResult<u64>> + Send {
        ready(self.with(|st| {
            Ok(st
                .repo(repo)?
                .workspaces
                .values()
                .filter(|ws| before_ms.is_none_or(|b| ws.lease_until_ms < b))
                .count() as u64)
        }))
    }

    fn derived(
        &self,
        repo: RepoId,
        kind: &str,
        version: u32,
        key: &[u8],
    ) -> impl Future<Output = StoreResult<Option<Vec<u8>>>> + Send {
        ready(self.with(|st| {
            let k = (kind.to_owned(), version, key.to_vec());
            Ok(st.repo(repo)?.derived.get(&k).cloned())
        }))
    }

    fn line_pos(
        &self,
        repo: RepoId,
        line: &str,
        id: ChangesetId,
    ) -> impl Future<Output = StoreResult<Option<u64>>> + Send {
        ready(self.with(|st| Ok(st.repo(repo)?.line_pos.get(&(line.to_owned(), id)).copied())))
    }

    fn scan_line_path(
        &self,
        repo: RepoId,
        line: &str,
        path: &RepoPath,
        before: u64,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(u64, ChangesetId)>>> + Send {
        ready(self.with(|st| {
            let rows = st
                .repo(repo)?
                .line_paths
                .get(&(line.to_owned(), path.clone()));
            Ok(rows
                .into_iter()
                .flatten()
                .rev()
                .filter(|(pos, _)| *pos < before)
                .take(page_limit(limit))
                .copied()
                .collect())
        }))
    }

    fn holders(
        &self,
        repo: RepoId,
        object: ObjectId,
    ) -> impl Future<Output = StoreResult<u32>> + Send {
        ready(self.with(|st| {
            let count = st
                .repo(repo)?
                .holders
                .range((object, String::new())..)
                .take_while(|(o, _)| *o == object)
                .count();
            Ok(u32::try_from(count).unwrap_or(u32::MAX))
        }))
    }

    fn land(
        &self,
        repo: RepoId,
        draft: ChangesetId,
        line: &str,
    ) -> impl Future<Output = StoreResult<Option<ChangesetId>>> + Send {
        ready(self.with(|st| Ok(st.repo(repo)?.lands.get(&(draft, line.to_owned())).copied())))
    }

    fn journal(
        &self,
        repo: RepoId,
        from: Lsn,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(Lsn, String, Vec<u8>)>>> + Send {
        ready(self.with(|st| {
            // Entry `i` has LSN `i + 1`, so `from` maps to index `from - 1`.
            let start = from.saturating_sub(1) as usize;
            Ok(st
                .repo(repo)?
                .journal
                .iter()
                .skip(start)
                .take(page_limit(limit))
                .cloned()
                .collect())
        }))
    }

    fn list_records(
        &self,
        repo: RepoId,
        state: RecordState,
        after: Option<ChangesetId>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(ChangesetId, ChangesetRecord)>>> + Send {
        ready(self.with(|st| {
            use std::ops::Bound;
            let start = after.map_or(Bound::Unbounded, Bound::Excluded);
            Ok(st
                .repo(repo)?
                .changesets
                .range((start, Bound::Unbounded))
                .filter(|(_, (_, record))| record.state == state)
                .take(page_limit(limit))
                .map(|(id, (_, record))| (*id, record.clone()))
                .collect())
        }))
    }

    fn holder_rows(
        &self,
        repo: RepoId,
        object: ObjectId,
        after: Option<String>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<String>>> + Send {
        ready(self.with(|st| {
            use std::ops::Bound;
            let start = match after {
                Some(name) => Bound::Excluded((object, name)),
                None => Bound::Included((object, String::new())),
            };
            Ok(st
                .repo(repo)?
                .holders
                .range((start, Bound::Unbounded))
                .take_while(|(o, _)| *o == object)
                .take(page_limit(limit))
                .map(|(_, holder)| holder.clone())
                .collect())
        }))
    }

    fn list_all_workspaces(
        &self,
        repo: RepoId,
        after: &str,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send {
        ready(self.with(|st| {
            use std::ops::Bound;
            Ok(st
                .repo(repo)?
                .workspaces
                .range::<str, _>((Bound::Excluded(after), Bound::Unbounded))
                .take(page_limit(limit))
                .map(|(_, ws)| ws.clone())
                .collect())
        }))
    }
}
