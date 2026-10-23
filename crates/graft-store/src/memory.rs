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
    ChangesetRecord, DoomMark, Lsn, MetaStore, MetaTxn, Precondition, Protection, RecordState,
    RefRecord, TimedRow, TxnOutcome, WorkspaceRecord, Write,
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
    /// Line → (parent line, fork position).
    line_forks: BTreeMap<String, (String, u64)>,
    holders: BTreeSet<(ObjectId, String)>,
    /// The expiry of each timed holder row.
    row_expiry: BTreeMap<(ObjectId, String), i64>,
    /// Protection by changeset; an absent entry is the default.
    protection: HashMap<ChangesetId, Protection>,
    doomed: BTreeMap<ObjectId, DoomMark>,
    gc_protection: bool,
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
            Precondition::RecordLive(id) => self
                .changesets
                .get(id)
                .is_some_and(|(_, record)| record.state != RecordState::Collected),
            Precondition::RecordIs {
                id,
                record,
                protection,
            } => {
                self.changesets.get(id).is_some_and(|(_, r)| r == record)
                    && self.protection_of(id) == *protection
            }
            Precondition::ObjectDoomed(object) => {
                self.doomed.get(object).is_some_and(|mark| !mark.deleting)
            }
            Precondition::ObjectUnheld(object) => self.rows_of(*object).next().is_none(),
            Precondition::DerivedIs {
                kind,
                version,
                key,
                value,
            } => self.derived.get(&(kind.clone(), *version, key.clone())) == value.as_ref(),
        }
    }

    fn protection_of(&self, id: &ChangesetId) -> Protection {
        self.protection.get(id).copied().unwrap_or_default()
    }

    fn state_of(&self, id: &ChangesetId) -> Option<RecordState> {
        self.changesets.get(id).map(|(_, record)| record.state)
    }

    /// The holders of `object`.
    fn rows_of(&self, object: ObjectId) -> impl Iterator<Item = &String> {
        self.holders
            .range((object, String::new())..)
            .take_while(move |(o, _)| *o == object)
            .map(|(_, holder)| holder)
    }
}

/// Whether a record in `state` keeps protection: landed and collected
/// records have none.
fn protectable(state: RecordState) -> bool {
    !matches!(state, RecordState::Landed | RecordState::Collected)
}

fn invalid(what: String) -> StoreError {
    StoreError::Invalid(what)
}

/// The protection and object state a transaction's writes leave, as far as
/// its checks need, over the stored state.
struct Overlay<'r> {
    repo: &'r Repo,
    states: HashMap<ChangesetId, RecordState>,
    pins: HashMap<ChangesetId, u32>,
    rows: HashMap<(ObjectId, &'r str), bool>,
    doomed: HashMap<ObjectId, Option<DoomMark>>,
}

impl<'r> Overlay<'r> {
    fn new(repo: &'r Repo) -> Self {
        Self {
            repo,
            states: HashMap::new(),
            pins: HashMap::new(),
            rows: HashMap::new(),
            doomed: HashMap::new(),
        }
    }

    fn state(&self, id: &ChangesetId) -> Option<RecordState> {
        self.states
            .get(id)
            .copied()
            .or_else(|| self.repo.state_of(id))
    }

    fn pins(&self, id: &ChangesetId) -> u32 {
        self.pins
            .get(id)
            .copied()
            .unwrap_or_else(|| self.repo.protection_of(id).pins)
    }

    fn mark(&self, object: &ObjectId) -> Option<DoomMark> {
        match self.doomed.get(object) {
            Some(mark) => *mark,
            None => self.repo.doomed.get(object).copied(),
        }
    }

    fn held(&self, object: ObjectId) -> bool {
        let added = self
            .rows
            .iter()
            .any(|((o, _), present)| *o == object && *present);
        added
            || self
                .repo
                .rows_of(object)
                .any(|h| self.rows.get(&(object, h.as_str())) != Some(&false))
    }

    fn add_row(&mut self, object: ObjectId, holder: &'r str) {
        self.rows.insert((object, holder), true);
        if self.mark(&object).is_some_and(|m| !m.deleting) {
            self.doomed.insert(object, None);
        }
    }

    /// Checks one protection or object write and records its effect.
    fn check(&mut self, write: &'r Write) -> StoreResult<()> {
        match write {
            Write::PutChangeset { id, record, .. } => {
                self.states.insert(*id, record.state);
                self.pins.insert(*id, 0);
            }
            Write::SetRecord { id, record } => {
                if self.state(id).is_none() {
                    return Err(invalid(format!("no changeset {id} to update")));
                }
                self.states.insert(*id, record.state);
                if !protectable(record.state) {
                    self.pins.insert(*id, 0);
                }
            }
            Write::SetExpiry { id, expires_at_ms } => match self.state(id) {
                None => return Err(invalid(format!("no changeset {id} to expire"))),
                Some(state) if !protectable(state) && expires_at_ms.is_some() => {
                    return Err(invalid(format!("changeset {id} is {state:?}: no expiry")));
                }
                Some(_) => {}
            },
            Write::Pin(id) => match self.state(id) {
                None | Some(RecordState::Collected) => {
                    return Err(invalid(format!("no live changeset {id} to pin")));
                }
                Some(RecordState::Landed) => {}
                Some(_) => {
                    let pins = self
                        .pins(id)
                        .checked_add(1)
                        .ok_or_else(|| invalid(format!("changeset {id} has too many pins")))?;
                    self.pins.insert(*id, pins);
                }
            },
            Write::Unpin(id) => match self.state(id) {
                None | Some(RecordState::Collected) => {
                    return Err(invalid(format!("no live changeset {id} to unpin")));
                }
                Some(RecordState::Landed) => {}
                Some(_) => {
                    self.pins.insert(*id, self.pins(id).saturating_sub(1));
                }
            },
            Write::SetPins { id, pins } => match self.state(id) {
                None => return Err(invalid(format!("no changeset {id} to pin"))),
                Some(state) if !protectable(state) && *pins > 0 => {
                    return Err(invalid(format!("changeset {id} is {state:?}: no pins")));
                }
                Some(_) => {
                    self.pins.insert(*id, *pins);
                }
            },
            Write::AddHolder { object, holder } | Write::AddHolderUntil { object, holder, .. } => {
                self.add_row(*object, holder);
            }
            Write::RemoveHolder { object, holder } => {
                self.rows.insert((*object, holder), false);
            }
            Write::Doom { object, since_ms } => {
                if self.held(*object) {
                    return Err(invalid(format!("object {} is held", object.to_hex())));
                }
                match self.mark(object) {
                    Some(mark) if mark.deleting => {
                        return Err(invalid(format!(
                            "object {} is being deleted",
                            object.to_hex()
                        )));
                    }
                    Some(_) => {}
                    None => {
                        self.doomed.insert(
                            *object,
                            Some(DoomMark {
                                since_ms: *since_ms,
                                deleting: false,
                            }),
                        );
                    }
                }
            }
            Write::MarkDeleting(object) => match self.mark(object) {
                None => return Err(invalid(format!("object {} is not doomed", object.to_hex()))),
                Some(mark) => {
                    self.doomed.insert(
                        *object,
                        Some(DoomMark {
                            deleting: true,
                            ..mark
                        }),
                    );
                }
            },
            Write::Undoom(object) => {
                self.doomed.insert(*object, None);
            }
            _ => {}
        }
        Ok(())
    }
}

/// Checks a transaction's writes against the state it would apply to, so
/// that applying them cannot fail.
fn validate(repo: &Repo, txn: &MetaTxn) -> StoreResult<()> {
    // Versions as the writes so far would leave them.
    let mut versions: HashMap<&str, u64> = HashMap::new();
    let mut overlay = Overlay::new(repo);
    for write in &txn.writes {
        overlay.check(write)?;
        if let Write::PutLineFork { line, .. } = write {
            // As SQLite's primary key does: one fork per line, ever.
            let twice = txn
                .writes
                .iter()
                .filter(|w| matches!(w, Write::PutLineFork { line: l, .. } if l == line))
                .count()
                > 1;
            if twice || repo.line_forks.contains_key(line) {
                return Err(StoreError::Invalid(format!(
                    "line {line} already has a fork"
                )));
            }
        }
        if let Write::SetRef(record) = write {
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
                repo.protection.remove(&id);
            }
            Write::SetRecord { id, mut record } => {
                if has_journal {
                    record.lsn = first_lsn;
                }
                if !protectable(record.state) {
                    repo.protection.remove(&id);
                }
                // Validated above: the changeset exists by now.
                if let Some(slot) = repo.changesets.get_mut(&id) {
                    slot.1 = record;
                }
            }
            Write::SetExpiry { id, expires_at_ms } => {
                repo.protection.entry(id).or_default().expires_at_ms = expires_at_ms;
            }
            Write::Pin(id) => {
                if repo.state_of(&id).is_some_and(protectable) {
                    repo.protection.entry(id).or_default().pins += 1;
                }
            }
            Write::Unpin(id) => {
                if repo.state_of(&id).is_some_and(protectable) {
                    let p = repo.protection.entry(id).or_default();
                    p.pins = p.pins.saturating_sub(1);
                }
            }
            Write::SetPins { id, pins } => {
                repo.protection.entry(id).or_default().pins = pins;
            }
            Write::AddHolderUntil {
                object,
                holder,
                expires_at_ms,
            } => {
                revive(repo, object);
                let row = (object, holder);
                if !repo.holders.contains(&row) {
                    repo.row_expiry.insert(row.clone(), expires_at_ms);
                    repo.holders.insert(row);
                }
            }
            Write::Doom { object, since_ms } => {
                repo.doomed.entry(object).or_insert(DoomMark {
                    since_ms,
                    deleting: false,
                });
            }
            Write::MarkDeleting(object) => {
                if let Some(mark) = repo.doomed.get_mut(&object) {
                    mark.deleting = true;
                }
            }
            Write::Undoom(object) => {
                repo.doomed.remove(&object);
            }
            Write::SetGcProtection(on) => repo.gc_protection = on,
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
            Write::PutLineFork { line, parent, pos } => {
                repo.line_forks.insert(line, (parent, pos));
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
                revive(repo, object);
                repo.holders.insert((object, holder));
            }
            Write::RemoveHolder { object, holder } => {
                let row = (object, holder);
                repo.row_expiry.remove(&row);
                repo.holders.remove(&row);
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

/// Clears a doomed mark that is not `deleting`: the object is held again.
fn revive(repo: &mut Repo, object: ObjectId) {
    if repo.doomed.get(&object).is_some_and(|m| !m.deleting) {
        repo.doomed.remove(&object);
    }
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
            // A new repository has protection records from its first write.
            let repo = Repo {
                gc_protection: true,
                ..Repo::default()
            };
            st.repos.insert(id, repo);
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
        ready(self.with(|st| {
            let st = st.repo(repo)?;
            let mut line = line.to_owned();
            let mut bound = u64::MAX;
            loop {
                if let Some(&pos) = st.line_pos.get(&(line.clone(), id)) {
                    return Ok((pos <= bound).then_some(pos));
                }
                let Some((parent, at)) = st.line_forks.get(&line) else {
                    return Ok(None);
                };
                bound = bound.min(*at);
                line = parent.clone();
            }
        }))
    }

    fn line_holders(
        &self,
        repo: RepoId,
        id: ChangesetId,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(String, u64)>>> + Send {
        // A test store: a scan of the positions stands in for an index.
        ready(self.with(|st| {
            Ok(st
                .repo(repo)?
                .line_pos
                .iter()
                .filter(|((_, c), _)| *c == id)
                .take(page_limit(limit))
                .map(|((line, _), pos)| (line.clone(), *pos))
                .collect())
        }))
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
            let st = st.repo(repo)?;
            let limit = page_limit(limit);
            let mut out = Vec::new();
            let mut line = line.to_owned();
            let mut before = before;
            loop {
                let rows = st.line_paths.get(&(line.clone(), path.clone()));
                out.extend(
                    rows.into_iter()
                        .flatten()
                        .rev()
                        .filter(|(pos, _)| *pos < before)
                        .take(limit - out.len())
                        .copied(),
                );
                if out.len() == limit {
                    return Ok(out);
                }
                let Some((parent, at)) = st.line_forks.get(&line) else {
                    return Ok(out);
                };
                before = before.min(at.saturating_add(1));
                line = parent.clone();
            }
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

    fn protection(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> impl Future<Output = StoreResult<Option<Protection>>> + Send {
        ready(self.with(|st| {
            let repo = st.repo(repo)?;
            Ok(repo
                .changesets
                .contains_key(&id)
                .then(|| repo.protection_of(&id)))
        }))
    }

    fn expired_records(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<(i64, ChangesetId)>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(i64, ChangesetId)>>> + Send {
        ready(self.with(|st| {
            let mut rows: Vec<(i64, ChangesetId)> = st
                .repo(repo)?
                .protection
                .iter()
                .filter(|(_, p)| p.pins == 0)
                .filter_map(|(id, p)| p.expires_at_ms.map(|at| (at, *id)))
                .filter(|row| row.0 <= now_ms && after.is_none_or(|a| *row > a))
                .collect();
            rows.sort();
            rows.truncate(page_limit(limit));
            Ok(rows)
        }))
    }

    fn held_objects(
        &self,
        repo: RepoId,
        holder: &str,
        after: Option<ObjectId>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<ObjectId>>> + Send {
        ready(self.with(|st| {
            Ok(st
                .repo(repo)?
                .holders
                .iter()
                .filter(|(o, h)| h == holder && after.is_none_or(|a| *o > a))
                .take(page_limit(limit))
                .map(|(o, _)| *o)
                .collect())
        }))
    }

    fn expired_rows(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<TimedRow>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<TimedRow>>> + Send {
        ready(self.with(|st| {
            let mut rows: Vec<TimedRow> = st
                .repo(repo)?
                .row_expiry
                .iter()
                .filter(|(_, at)| **at <= now_ms)
                .map(|((object, holder), at)| TimedRow {
                    expires_at_ms: *at,
                    object: *object,
                    holder: holder.clone(),
                })
                .filter(|row| after.as_ref().is_none_or(|a| row > a))
                .collect();
            rows.sort();
            rows.truncate(page_limit(limit));
            Ok(rows)
        }))
    }

    fn doom_marks(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
    ) -> impl Future<Output = StoreResult<Vec<(ObjectId, DoomMark)>>> + Send {
        ready(self.with(|st| {
            let repo = st.repo(repo)?;
            Ok(ids
                .iter()
                .filter_map(|id| repo.doomed.get(id).map(|mark| (*id, *mark)))
                .collect())
        }))
    }

    fn old_doomed(
        &self,
        repo: RepoId,
        until_ms: i64,
        after: Option<(i64, ObjectId)>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(ObjectId, DoomMark)>>> + Send {
        ready(self.with(|st| {
            let mut rows: Vec<(i64, ObjectId, DoomMark)> = st
                .repo(repo)?
                .doomed
                .iter()
                .filter(|(o, m)| {
                    m.since_ms <= until_ms && after.is_none_or(|a| (m.since_ms, **o) > a)
                })
                .map(|(o, m)| (m.since_ms, *o, *m))
                .collect();
            rows.sort_by_key(|(since, o, _)| (*since, *o));
            rows.truncate(page_limit(limit));
            Ok(rows.into_iter().map(|(_, o, m)| (o, m)).collect())
        }))
    }

    fn lapsed_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        after: Option<(i64, String)>,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send {
        ready(self.with(|st| {
            let mut rows: Vec<&WorkspaceRecord> = st
                .repo(repo)?
                .workspaces
                .values()
                .filter(|ws| ws.lease_until_ms < before_ms)
                .filter(|ws| {
                    after
                        .as_ref()
                        .is_none_or(|(at, name)| (ws.lease_until_ms, &ws.name) > (*at, name))
                })
                .collect();
            rows.sort_by(|a, b| (a.lease_until_ms, &a.name).cmp(&(b.lease_until_ms, &b.name)));
            Ok(rows.into_iter().take(page_limit(limit)).cloned().collect())
        }))
    }

    fn gc_protection(&self, repo: RepoId) -> impl Future<Output = StoreResult<bool>> + Send {
        ready(self.with(|st| Ok(st.repo(repo)?.gc_protection)))
    }
}
