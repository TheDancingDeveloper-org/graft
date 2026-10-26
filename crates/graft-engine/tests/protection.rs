//! G3.11: GC protection records (design §8.6), on both stores.
//!
//! - After any random sequence of commits, merges, rebases, syncs, lands,
//!   closes, lapses and adoptions, a brute-force check holds: every object
//!   in a changeset's snapshot has a row held by a changeset on its
//!   first-parent chain; pins on a non-landed record equal its children;
//!   landed records have no pins and no expiry; every record that is not a
//!   workspace's draft has an expiry.
//! - A write adds one pin per non-landed parent, counted, whatever the
//!   history behind it.
//! - Claims: a tree doomed before a create derives it is claimed and cannot
//!   be marked for deletion before the commit; one doomed after the claim
//!   is revived by the commit; a blob being deleted is `MissingObjects`, a
//!   tree being deleted is waited for and put again, an upload of one is
//!   `Unavailable`.
//! - A create racing its parent's collection, on real threads against
//!   SQLite, either pins the parent or fails `Collected`.
//! - The backfill on a repository written without protection (including a
//!   version 3 SQLite database) sets the pins and expiries, adds no rows,
//!   and reads no landed record.

mod common;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use common::*;
use graft_engine::{EngineConfig, LineConfig, ProtectReport, Sleeper};
use graft_model::{
    Changeset, ChangesetId, Clock, EntryKind, ObjectId, ObjectKind, RepoPath, Tree, TreeEntry,
};
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{ApiError, Frame, ObjectApi, RepoApi, WorkspaceApi};
use graft_store::memory::MemoryStore;
use graft_store::{
    ChangesetRecord, DoomMark, Lsn, MetaStore, MetaTxn, ObjectStore, Precondition, Protection,
    RecordState, RefRecord, RepoId, StoreResult, TimedRow, TxnOutcome, WorkspaceRecord, Write,
};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};
use proptest::prelude::*;

const LINE: &str = "trunk";
const DAY: i64 = 24 * 3600 * 1000;

// ---------------------------------------------------------------------------
// A spying metadata store: counts pin writes and record reads, and can run
// one transaction just before the next commit that stores a changeset.

#[derive(Default)]
struct Spy<M> {
    inner: M,
    /// `Write::Pin` in committed transactions.
    pins: AtomicU64,
    /// Every changeset `changeset` read.
    reads: Mutex<Vec<ChangesetId>>,
    /// Every state `list_records` listed.
    listed: Mutex<Vec<RecordState>>,
    /// Run before the next commit that stores a changeset, and its outcome.
    hook: Mutex<Option<MetaTxn>>,
    hooked: Mutex<Option<StoreResult<TxnOutcome>>>,
}

impl<M> Spy<M> {
    fn new(inner: M) -> Self {
        Self {
            inner,
            pins: AtomicU64::new(0),
            reads: Mutex::default(),
            listed: Mutex::default(),
            hook: Mutex::default(),
            hooked: Mutex::default(),
        }
    }

    fn pins(&self) -> u64 {
        self.pins.load(Ordering::SeqCst)
    }

    fn arm(&self, txn: MetaTxn) {
        *self.hook.lock().expect("hook") = Some(txn);
        *self.hooked.lock().expect("hooked") = None;
    }

    fn hooked(&self) -> Option<StoreResult<TxnOutcome>> {
        self.hooked.lock().expect("hooked").take()
    }
}

impl<M: MetaStore> MetaStore for Spy<M> {
    async fn create_repo(&self, name: &str) -> StoreResult<RepoId> {
        self.inner.create_repo(name).await
    }
    async fn repo(&self, name: &str) -> StoreResult<Option<RepoId>> {
        self.inner.repo(name).await
    }
    async fn commit(&self, repo: RepoId, txn: MetaTxn) -> StoreResult<TxnOutcome> {
        let stores = txn
            .writes
            .iter()
            .any(|w| matches!(w, Write::PutChangeset { .. }));
        if stores {
            let hook = self.hook.lock().expect("hook").take();
            if let Some(hook) = hook {
                let outcome = self.inner.commit(repo, hook).await;
                *self.hooked.lock().expect("hooked") = Some(outcome);
            }
        }
        let pins = txn
            .writes
            .iter()
            .filter(|w| matches!(w, Write::Pin(_)))
            .count() as u64;
        let outcome = self.inner.commit(repo, txn).await?;
        if matches!(outcome, TxnOutcome::Committed(_)) {
            self.pins.fetch_add(pins, Ordering::SeqCst);
        }
        Ok(outcome)
    }
    async fn changeset(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> StoreResult<Option<(Vec<u8>, ChangesetRecord)>> {
        self.reads.lock().expect("reads").push(id);
        self.inner.changeset(repo, id).await
    }
    async fn get_ref(&self, repo: RepoId, name: &str) -> StoreResult<Option<RefRecord>> {
        self.inner.get_ref(repo, name).await
    }
    async fn list_refs(
        &self,
        repo: RepoId,
        prefix: &str,
        limit: u32,
    ) -> StoreResult<Vec<RefRecord>> {
        self.inner.list_refs(repo, prefix, limit).await
    }
    async fn workspace(&self, repo: RepoId, name: &str) -> StoreResult<Option<WorkspaceRecord>> {
        self.inner.workspace(repo, name).await
    }
    async fn expired_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.inner.expired_workspaces(repo, before_ms, limit).await
    }
    async fn list_workspaces(
        &self,
        repo: RepoId,
        owner: &str,
        after: &str,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.inner.list_workspaces(repo, owner, after, limit).await
    }
    async fn count_records(&self, repo: RepoId, state: RecordState) -> StoreResult<u64> {
        self.inner.count_records(repo, state).await
    }
    async fn count_workspaces(&self, repo: RepoId, before_ms: Option<i64>) -> StoreResult<u64> {
        self.inner.count_workspaces(repo, before_ms).await
    }
    async fn derived(
        &self,
        repo: RepoId,
        kind: &str,
        version: u32,
        key: &[u8],
    ) -> StoreResult<Option<Vec<u8>>> {
        self.inner.derived(repo, kind, version, key).await
    }
    async fn line_pos(
        &self,
        repo: RepoId,
        line: &str,
        id: ChangesetId,
    ) -> StoreResult<Option<u64>> {
        self.inner.line_pos(repo, line, id).await
    }
    async fn line_holders(
        &self,
        repo: RepoId,
        id: ChangesetId,
        limit: u32,
    ) -> StoreResult<Vec<(String, u64)>> {
        self.inner.line_holders(repo, id, limit).await
    }
    async fn scan_line_path(
        &self,
        repo: RepoId,
        line: &str,
        path: &RepoPath,
        before: u64,
        limit: u32,
    ) -> StoreResult<Vec<(u64, ChangesetId)>> {
        self.inner
            .scan_line_path(repo, line, path, before, limit)
            .await
    }
    async fn holders(&self, repo: RepoId, object: ObjectId) -> StoreResult<u32> {
        self.inner.holders(repo, object).await
    }
    async fn land(
        &self,
        repo: RepoId,
        draft: ChangesetId,
        line: &str,
    ) -> StoreResult<Option<ChangesetId>> {
        self.inner.land(repo, draft, line).await
    }
    async fn journal(
        &self,
        repo: RepoId,
        from: Lsn,
        limit: u32,
    ) -> StoreResult<Vec<(Lsn, String, Vec<u8>)>> {
        self.inner.journal(repo, from, limit).await
    }
    async fn list_records(
        &self,
        repo: RepoId,
        state: RecordState,
        after: Option<ChangesetId>,
        limit: u32,
    ) -> StoreResult<Vec<(ChangesetId, ChangesetRecord)>> {
        self.listed.lock().expect("listed").push(state);
        self.inner.list_records(repo, state, after, limit).await
    }
    async fn list_all_workspaces(
        &self,
        repo: RepoId,
        after: &str,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.inner.list_all_workspaces(repo, after, limit).await
    }
    async fn holder_rows(
        &self,
        repo: RepoId,
        object: ObjectId,
        after: Option<String>,
        limit: u32,
    ) -> StoreResult<Vec<String>> {
        self.inner.holder_rows(repo, object, after, limit).await
    }
    async fn protection(&self, repo: RepoId, id: ChangesetId) -> StoreResult<Option<Protection>> {
        self.inner.protection(repo, id).await
    }
    async fn expired_records(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<(i64, ChangesetId)>,
        limit: u32,
    ) -> StoreResult<Vec<(i64, ChangesetId)>> {
        self.inner.expired_records(repo, now_ms, after, limit).await
    }
    async fn held_objects(
        &self,
        repo: RepoId,
        holder: &str,
        after: Option<ObjectId>,
        limit: u32,
    ) -> StoreResult<Vec<ObjectId>> {
        self.inner.held_objects(repo, holder, after, limit).await
    }
    async fn expired_rows(
        &self,
        repo: RepoId,
        now_ms: i64,
        after: Option<TimedRow>,
        limit: u32,
    ) -> StoreResult<Vec<TimedRow>> {
        self.inner.expired_rows(repo, now_ms, after, limit).await
    }
    async fn doom_marks(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
    ) -> StoreResult<Vec<(ObjectId, DoomMark)>> {
        self.inner.doom_marks(repo, ids).await
    }
    async fn old_doomed(
        &self,
        repo: RepoId,
        until_ms: i64,
        after: Option<(i64, ObjectId)>,
        limit: u32,
    ) -> StoreResult<Vec<(ObjectId, DoomMark)>> {
        self.inner.old_doomed(repo, until_ms, after, limit).await
    }
    async fn lapsed_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        after: Option<(i64, String)>,
        limit: u32,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        self.inner
            .lapsed_workspaces(repo, before_ms, after, limit)
            .await
    }
    async fn gc_protection(&self, repo: RepoId) -> StoreResult<bool> {
        self.inner.gc_protection(repo).await
    }
    async fn journal_tip(&self, repo: RepoId) -> StoreResult<Option<Lsn>> {
        self.inner.journal_tip(repo).await
    }
}

fn spy_memory() -> (Fixture<Spy<MemoryStore>>, Arc<Spy<MemoryStore>>) {
    let meta = Arc::new(Spy::new(MemoryStore::new()));
    (build(meta.clone(), config(), None), meta)
}

fn spy_sqlite() -> (Fixture<Spy<SqliteMetaStore>>, Arc<Spy<SqliteMetaStore>>) {
    let dir = TempDir::new("protect");
    let store = SqliteMetaStore::open(SqliteConfig::new(dir_db(&dir))).expect("open sqlite");
    let meta = Arc::new(Spy::new(store));
    (build(meta.clone(), config(), Some(dir)), meta)
}

fn dir_db(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join("meta.sqlite")
}

/// Generates one `#[test]` per store for each scenario taking a fixture over
/// a [`Spy`] and the spy itself.
macro_rules! on_both_spies {
    ($group:ident: $($name:ident),* $(,)?) => {
        mod $group {
            mod memory_store {
                $( #[test] fn $name() { let (f, spy) = super::super::spy_memory(); super::super::$name(&f, &spy) } )*
            }
            mod sqlite_store {
                $( #[test] fn $name() { let (f, spy) = super::super::spy_sqlite(); super::super::$name(&f, &spy) } )*
            }
        }
    };
}

fn config() -> EngineConfig {
    EngineConfig {
        lines: vec![LineConfig::direct(LINE)],
        ..EngineConfig::default()
    }
}

// ---------------------------------------------------------------------------
// The brute-force check.

fn repo_id<M: MetaStore>(r: &Repo<'_, M>) -> RepoId {
    block_on(r.e().stores().0.repo(&r.name))
        .expect("repo")
        .expect("exists")
}

struct Stored {
    id: ChangesetId,
    record: ChangesetRecord,
    cs: Changeset,
    protection: Protection,
}

fn all_records<M: MetaStore>(meta: &M, repo: RepoId) -> Vec<Stored> {
    let mut out = Vec::new();
    for state in [
        RecordState::Draft,
        RecordState::Landed,
        RecordState::Superseded,
        RecordState::Abandoned,
        RecordState::Collected,
    ] {
        let mut after = None;
        loop {
            let page = block_on(meta.list_records(repo, state, after, 100)).expect("list");
            let Some((last, _)) = page.last() else {
                break;
            };
            after = Some(*last);
            for (id, record) in page {
                let (content, _) = block_on(meta.changeset(repo, id))
                    .expect("read")
                    .expect("stored");
                let protection = block_on(meta.protection(repo, id))
                    .expect("protection")
                    .expect("stored");
                out.push(Stored {
                    id,
                    record,
                    cs: Changeset::decode(&content).expect("decode"),
                    protection,
                });
            }
        }
    }
    out
}

fn rows_of<M: MetaStore>(meta: &M, repo: RepoId, holder: &str) -> HashSet<ObjectId> {
    let mut out = HashSet::new();
    let mut after = None;
    loop {
        let page = block_on(meta.held_objects(repo, holder, after, 100)).expect("held");
        let Some(last) = page.last() else {
            return out;
        };
        after = Some(*last);
        out.extend(page);
    }
}

/// Every object in the snapshot rooted at `root`, by walking all of it.
fn snapshot(objects: &Faulty, repo: RepoId, root: ObjectId) -> HashSet<ObjectId> {
    let mut out = HashSet::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if !out.insert(id) {
            continue;
        }
        let bytes = block_on(objects.get(repo, id))
            .expect("get")
            .unwrap_or_else(|| panic!("tree {} is stored", id.to_hex()));
        let payload = Frame::from_bytes(&bytes)
            .expect("frame")
            .decode_verified(id)
            .expect("verified");
        for e in Tree::decode(&payload).expect("tree").entries {
            match e.kind {
                EntryKind::Tree => stack.push(e.id),
                EntryKind::Submodule => {}
                _ => {
                    out.insert(e.id);
                }
            }
        }
    }
    out
}

/// Checks design §8.6 over every record of the repository, by brute force.
fn check<M: MetaStore>(r: &Repo<'_, M>) -> Result<(), String> {
    let meta = r.e().stores().0;
    let repo = repo_id(r);
    let records = all_records(meta, repo);
    let by_id: HashMap<ChangesetId, &Stored> = records.iter().map(|s| (s.id, s)).collect();

    // Pins: on a non-landed record, its children that are neither landed
    // nor collected (every pin comes from a non-landed child); none on a
    // landed record.
    let mut children: HashMap<ChangesetId, u32> = HashMap::new();
    for s in &records {
        if matches!(s.record.state, RecordState::Collected | RecordState::Landed) {
            continue;
        }
        let mut seen = HashSet::new();
        for p in &s.cs.parents {
            if seen.insert(*p) {
                *children.entry(*p).or_default() += 1;
            }
        }
    }
    for s in &records {
        let want = match s.record.state {
            RecordState::Landed => 0,
            _ => children.get(&s.id).copied().unwrap_or(0),
        };
        if s.protection.pins != want {
            return Err(format!(
                "{} ({:?}) has {} pins, not {want}",
                s.id, s.record.state, s.protection.pins
            ));
        }
        // Expiry: none for a workspace's draft or a landed record; set for
        // everything else, land-attempt candidates included.
        let workspace_draft = s.record.state == RecordState::Draft
            && s.record
                .holder
                .as_deref()
                .is_some_and(|h| h.starts_with("ws:"));
        let none = workspace_draft || s.record.state == RecordState::Landed;
        if s.protection.expires_at_ms.is_some() == none {
            return Err(format!(
                "{} ({:?}, held by {:?}) has expiry {:?}",
                s.id, s.record.state, s.record.holder, s.protection.expires_at_ms
            ));
        }
        if s.record
            .holder
            .as_deref()
            .is_some_and(|h| h.starts_with("land:"))
            && s.record.state == RecordState::Draft
            && s.protection.expires_at_ms.is_none()
        {
            return Err(format!("candidate {} has no expiry", s.id));
        }
    }

    // The invariant: every object in a changeset's snapshot has a row held
    // by a changeset on its first-parent chain (each pinned by the one
    // above it, or landed).
    let mut rows: HashMap<ChangesetId, HashSet<ObjectId>> = HashMap::new();
    for s in &records {
        rows.insert(s.id, rows_of(meta, repo, &s.id.to_string()));
    }
    for s in &records {
        let mut held: HashSet<ObjectId> = HashSet::new();
        let mut at = Some(s.id);
        while let Some(id) = at {
            let link = by_id
                .get(&id)
                .ok_or_else(|| format!("{id} on the chain of {} is not stored", s.id))?;
            held.extend(rows[&id].iter().copied());
            if link.record.state != RecordState::Landed && id != s.id && link.protection.pins == 0 {
                return Err(format!("{id} on the chain of {} has no pin", s.id));
            }
            at = link.cs.first_parent();
        }
        let unheld: Vec<String> = snapshot(&r.f.objects, repo, s.cs.root.0)
            .into_iter()
            .filter(|o| !held.contains(o))
            .map(|o| o.to_hex())
            .collect();
        if !unheld.is_empty() {
            return Err(format!("{} reaches unheld objects {unheld:?}", s.id));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Workspace and line shortcuts.

fn wref<M>(r: &Repo<'_, M>, ws: &str) -> WorkspaceRef {
    WorkspaceRef {
        repo: r.name.clone(),
        name: ws.to_owned(),
    }
}

fn open<M: MetaStore>(r: &Repo<'_, M>, ws: &str) -> ApiResult<WorkspaceView> {
    block_on(r.e().open_workspace(
        &r.cx,
        OpenWorkspace {
            repo: r.name.clone(),
            name: Some(ws.to_owned()),
            line: LINE.to_owned(),
            at: None,
            lease_ms: 0,
        },
    ))
}

fn workspace<M: MetaStore>(r: &Repo<'_, M>, ws: &str) -> Option<WorkspaceView> {
    block_on(r.e().get_workspace(&r.cx, wref(r, ws))).ok()
}

fn commit_in<M: MetaStore>(
    r: &Repo<'_, M>,
    ws: &str,
    parents: Vec<ChangesetId>,
    path: &str,
    data: &[u8],
) -> ApiResult<Created> {
    r.upload(&[data]);
    let mut req = commit_req(&r.name, parents, vec![put(path, data)]);
    req.workspace = Some(ws.to_owned());
    block_on(r.e().create_changeset(&r.cx, req))
}

fn land<M: MetaStore>(r: &Repo<'_, M>, head: ChangesetId) -> ApiResult<Landed> {
    block_on(r.e().land(
        &r.cx,
        Land {
            repo: r.name.clone(),
            head,
            line: LINE.to_owned(),
            squash: false,
            merge: MergeMode::File,
            deadline_ms: None,
            merged_from: None,
        },
    ))
}

fn tip<M: MetaStore>(r: &Repo<'_, M>) -> ChangesetId {
    block_on(r.e().resolve_ref(
        &r.cx,
        ResolveRef {
            repo: r.name.clone(),
            name: LINE.to_owned(),
        },
    ))
    .expect("line")
    .target
}

/// A repository whose line is at a landed root with files `a`, `b` and `c`.
fn seeded<'f, M: MetaStore>(f: &'f Fixture<M>) -> (Repo<'f, M>, ChangesetId) {
    let r = Repo::new(f, "r");
    let root = r
        .commit(vec![], &[("a", b"a0"), ("b", b"b0"), ("d/c", b"c0")], &[])
        .id;
    r.update_ref(LINE, None, root, RefKind::Line).expect("line");
    (r, root)
}

fn protection_of<M: MetaStore>(r: &Repo<'_, M>, id: ChangesetId) -> Protection {
    block_on(r.e().stores().0.protection(repo_id(r), id))
        .expect("protection")
        .expect("stored")
}

fn commit_txn<M: MetaStore>(r: &Repo<'_, M>, writes: Vec<Write>) {
    let outcome = block_on(r.e().stores().0.commit(
        repo_id(r),
        MetaTxn {
            preconditions: Vec::new(),
            writes,
        },
    ))
    .expect("commit");
    assert!(matches!(outcome, TxnOutcome::Committed(_)), "{outcome:?}");
}

// ---------------------------------------------------------------------------
// The model test.

const PATHS: &[&str] = &["a", "b", "d/c", "d/e/f", "g"];
const WORKSPACES: &[&str] = &["w0", "w1"];

#[derive(Clone, Debug)]
enum Op {
    Commit {
        ws: usize,
        path: usize,
    },
    Merge {
        ws: usize,
        other: usize,
        path: usize,
    },
    Rebase {
        ws: usize,
    },
    Sync {
        ws: usize,
    },
    Land {
        ws: usize,
    },
    Close {
        ws: usize,
    },
    Drop {
        ws: usize,
    },
    Direct {
        path: usize,
    },
    Adopt {
        ws: usize,
    },
    Lapse,
}

fn op() -> impl Strategy<Value = Op> {
    let ws = 0..WORKSPACES.len();
    let path = 0..PATHS.len();
    prop_oneof![
        8 => (ws.clone(), path.clone()).prop_map(|(ws, path)| Op::Commit { ws, path }),
        2 => (ws.clone(), ws.clone(), path.clone())
            .prop_map(|(ws, other, path)| Op::Merge { ws, other, path }),
        2 => ws.clone().prop_map(|ws| Op::Rebase { ws }),
        3 => ws.clone().prop_map(|ws| Op::Sync { ws }),
        4 => ws.clone().prop_map(|ws| Op::Land { ws }),
        1 => ws.clone().prop_map(|ws| Op::Close { ws }),
        1 => ws.clone().prop_map(|ws| Op::Drop { ws }),
        2 => path.prop_map(|path| Op::Direct { path }),
        2 => ws.prop_map(|ws| Op::Adopt { ws }),
        1 => Just(Op::Lapse),
    ]
}

/// Runs `ops`, ignoring the errors the engine may rightly return (a
/// conflict, a stale head), and checks the protection after each.
fn run_ops<M: MetaStore>(f: &Fixture<M>, ops: &[Op]) -> Result<(), TestCaseError> {
    let (r, _) = seeded(f);
    let mut direct: Option<ChangesetId> = None;
    let ensure = |ws: usize| -> WorkspaceView {
        let name = WORKSPACES[ws];
        match workspace(&r, name) {
            Some(view) if view.lease_until_ms >= f.clock.now().ms => view,
            _ => {
                let _ = block_on(r.e().close_workspace(&r.cx, wref(&r, name), false));
                open(&r, name).expect("open")
            }
        }
    };
    for (n, op) in ops.iter().enumerate() {
        let data = format!("v{n}").into_bytes();
        match *op {
            Op::Commit { ws, path } => {
                let view = ensure(ws);
                let parent = view.head.unwrap_or(view.base);
                let _ = commit_in(&r, WORKSPACES[ws], vec![parent], PATHS[path], &data);
            }
            Op::Merge { ws, other, path } => {
                let view = ensure(ws);
                let second = ensure(other).head;
                if let Some(second) = second.filter(|_| other != ws) {
                    let parent = view.head.unwrap_or(view.base);
                    let _ = commit_in(&r, WORKSPACES[ws], vec![parent, second], PATHS[path], &data);
                }
            }
            Op::Rebase { ws } => {
                if let Some(head) = ensure(ws).head {
                    let _ = block_on(r.e().rebase(
                        &r.cx,
                        Rebase {
                            repo: r.name.clone(),
                            changeset: head,
                            onto: tip(&r),
                            merge: MergeMode::File,
                            dry_run: false,
                        },
                    ));
                }
            }
            Op::Sync { ws } => {
                ensure(ws);
                let _ = block_on(r.e().sync(&r.cx, wref(&r, WORKSPACES[ws]), None));
            }
            Op::Land { ws } => {
                if let Some(head) = ensure(ws).head {
                    let _ = land(&r, head);
                }
            }
            Op::Close { ws } => {
                ensure(ws);
                block_on(
                    r.e()
                        .close_workspace(&r.cx, wref(&r, WORKSPACES[ws]), false),
                )
                .expect("close");
            }
            Op::Drop { ws } => {
                let view = ensure(ws);
                let _ = block_on(r.e().set_head(
                    &r.cx,
                    SetHead {
                        workspace: wref(&r, WORKSPACES[ws]),
                        expected: view.head,
                        new: None,
                    },
                ));
            }
            Op::Direct { path } => {
                r.upload(&[&data]);
                direct = r
                    .try_commit(vec![tip(&r)], vec![put(PATHS[path], &data)])
                    .ok()
                    .map(|c| c.id);
            }
            Op::Adopt { ws } => {
                let view = ensure(ws);
                if let Some(id) = direct {
                    let _ = block_on(r.e().set_head(
                        &r.cx,
                        SetHead {
                            workspace: wref(&r, WORKSPACES[ws]),
                            expected: view.head,
                            new: Some(id),
                        },
                    ));
                }
            }
            Op::Lapse => {
                f.clock.advance(3 * 3600 * 1000);
                block_on(r.e().expire_leases(&r.name, 100)).expect("expire");
            }
        }
        if let Err(e) = check(&r) {
            return Err(TestCaseError::fail(format!("after op {n} {op:?}: {e}")));
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn protection_holds_after_any_sequence_on_memory(
        ops in prop::collection::vec(op(), 1..32)
    ) {
        run_ops(&memory_with(config()), &ops)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]

    #[test]
    fn protection_holds_after_any_sequence_on_sqlite(
        ops in prop::collection::vec(op(), 1..24)
    ) {
        run_ops(&sqlite_with(config()), &ops)?;
    }
}

// ---------------------------------------------------------------------------
// Pins and expiries, step by step.

fn pins_and_expiries_follow_the_records<M: MetaStore>(f: &Fixture<Spy<M>>, spy: &Spy<M>) {
    let (r, root) = seeded(f);
    let now = f.clock.now().ms;
    let retention = config().draft_retention_ms as i64;

    open(&r, "w").expect("open");
    let a = commit_in(&r, "w", vec![root], "a", b"a1").expect("a").id;
    let b = commit_in(&r, "w", vec![a], "b", b"b1").expect("b").id;
    assert_eq!(protection_of(&r, root), Protection::default(), "landed");
    assert_eq!(
        protection_of(&r, a),
        Protection {
            pins: 1,
            expires_at_ms: None
        }
    );
    assert_eq!(protection_of(&r, b), Protection::default());

    // A changeset nothing holds is abandoned at once, and expires.
    r.upload(&[b"x"]);
    let x = r.try_commit(vec![b], vec![put("x", b"x")]).expect("x").id;
    assert_eq!(protection_of(&r, b).pins, 1);
    assert_eq!(protection_of(&r, x).expires_at_ms, Some(now + retention));

    // A merge pins both of its non-landed parents, each once.
    open(&r, "v").expect("open");
    let m = commit_in(&r, "v", vec![root, b], "g", b"g1")
        .expect("merge")
        .id;
    assert_eq!(protection_of(&r, b).pins, 2);
    assert_eq!(protection_of(&r, root).pins, 0);

    // Moving the head down abandons the top: it expires from then.
    f.clock.advance(1000);
    block_on(r.e().set_head(
        &r.cx,
        SetHead {
            workspace: wref(&r, "w"),
            expected: Some(b),
            new: Some(a),
        },
    ))
    .expect("drop b");
    assert_eq!(
        protection_of(&r, b).expires_at_ms,
        Some(now + 1000 + retention)
    );
    // Adopting it again clears the expiry.
    block_on(r.e().set_head(
        &r.cx,
        SetHead {
            workspace: wref(&r, "w"),
            expected: Some(a),
            new: Some(b),
        },
    ))
    .expect("adopt b");
    assert_eq!(protection_of(&r, b).expires_at_ms, None);

    // A lapsed lease ends at `lease_until`, not when it is noticed.
    let lease_until = workspace(&r, "w").expect("w").lease_until_ms;
    f.clock.advance(5 * DAY);
    block_on(r.e().expire_leases(&r.name, 10)).expect("expire");
    assert_eq!(
        protection_of(&r, a).expires_at_ms,
        Some(lease_until + retention)
    );
    assert_eq!(
        protection_of(&r, b).expires_at_ms,
        Some(lease_until + retention)
    );
    check(&r).expect("protection");

    // Landing the merge zeroes its pins and clears its expiry, and releases
    // its pin on `b`, which is off the line: only `x` pins `b` now.
    let before = spy.pins();
    let landed = land(&r, m).expect("land");
    assert!(matches!(landed, Landed::Landed { landed, .. } if landed == m));
    assert_eq!(spy.pins(), before, "landing pins nothing");
    assert_eq!(protection_of(&r, m), Protection::default());
    assert_eq!(protection_of(&r, b).pins, 1);
    check(&r).expect("protection");
}

/// A write adds one pin per non-landed parent, whatever the stack or the
/// history behind it.
fn a_write_adds_o_parents_pin_writes<M: MetaStore>(f: &Fixture<Spy<M>>, spy: &Spy<M>) {
    let (r, root) = seeded(f);
    let mut tip_id = root;
    for i in 0..40 {
        let data = format!("h{i}").into_bytes();
        r.upload(&[&data]);
        tip_id = r
            .try_commit(vec![tip_id], vec![put("a", &data)])
            .expect("history")
            .id;
    }
    r.update_ref(LINE, Some(root), tip_id, RefKind::Line)
        .expect("fast-forward");
    open(&r, "w").expect("open");
    let mut head = tip_id;
    let mut deltas = Vec::new();
    for i in 0..30 {
        let before = spy.pins();
        head = commit_in(&r, "w", vec![head], "b", format!("s{i}").as_bytes())
            .expect("stack")
            .id;
        deltas.push(spy.pins() - before);
    }
    assert_eq!(deltas[0], 0, "the first draft's parent has landed");
    assert!(
        deltas[1..].iter().all(|d| *d == 1),
        "one pin per draft: {deltas:?}"
    );

    // Two non-landed parents and a landed one: two pins.
    open(&r, "v").expect("open");
    let other = commit_in(&r, "v", vec![tip_id], "c", b"other")
        .expect("other")
        .id;
    let before = spy.pins();
    commit_in(&r, "w", vec![head, other, tip_id], "g", b"merge").expect("merge");
    assert_eq!(spy.pins() - before, 2);
}

/// A land candidate carries the land-attempt lease's expiry from the
/// moment it is written, and loses it when it lands.
fn candidates_expire_with_their_lease<M: MetaStore>(f: &Fixture<Spy<M>>, _spy: &Spy<M>) {
    let (r, root) = seeded(f);
    open(&r, "w").expect("open");
    let a = commit_in(&r, "w", vec![root], "a", b"a1").expect("a").id;
    // The line moves under the draft, so landing it rebases a candidate.
    r.upload(&[b"b1"]);
    let moved = r
        .try_commit(vec![root], vec![put("b", b"b1")])
        .expect("b")
        .id;
    r.update_ref(LINE, Some(root), moved, RefKind::Line)
        .expect("move");
    let landed = land(&r, a).expect("land");
    let Landed::Landed {
        landed: candidate,
        rebased: true,
        ..
    } = landed
    else {
        panic!("expected a rebased land: {landed:?}");
    };
    assert_eq!(protection_of(&r, candidate), Protection::default());
    check(&r).expect("protection");
}

#[test]
fn queued_candidates_keep_their_expiry_until_they_land() {
    // A queued line whose CI hook fails every candidate: the candidate is
    // written, then refused, and is left to its lease.
    let mut config = config();
    config.lines.push(LineConfig {
        policy: graft_engine::policy::LandPolicy::Queued {
            checks: vec!["ci".to_owned()],
        },
        ..LineConfig::direct("queued")
    });
    let f = memory_with(config.clone());
    let r = Repo::new(&f, "q");
    let root = r.commit(vec![], &[("a", b"a0")], &[]).id;
    r.update_ref("queued", None, root, RefKind::Line)
        .expect("line");
    r.upload(&[b"a1", b"b1"]);
    let draft = r
        .try_commit(vec![root], vec![put("a", b"a1")])
        .expect("draft")
        .id;
    let moved = r
        .try_commit(vec![root], vec![put("b", b"b1")])
        .expect("moved")
        .id;
    r.update_ref("queued", Some(root), moved, RefKind::Line)
        .expect("move");
    let now = f.clock.now().ms;
    let refused = block_on(r.e().land(
        &r.cx,
        Land {
            repo: r.name.clone(),
            head: draft,
            line: "queued".to_owned(),
            squash: false,
            merge: MergeMode::File,
            deadline_ms: None,
            merged_from: None,
        },
    ));
    assert!(
        matches!(refused, Err(ApiError::Rejected { .. })),
        "{refused:?}"
    );
    let meta = r.e().stores().0;
    let repo = repo_id(&r);
    let drafts = block_on(meta.list_records(repo, RecordState::Draft, None, 10)).expect("list");
    let candidates: Vec<_> = drafts
        .iter()
        .filter(|(_, rec)| {
            rec.holder
                .as_deref()
                .is_some_and(|h| h.starts_with("land:"))
        })
        .collect();
    assert_eq!(candidates.len(), 1, "{drafts:?}");
    assert_eq!(
        protection_of(&r, candidates[0].0).expires_at_ms,
        Some(now + config.land_lease_ms as i64)
    );
    check(&r).expect("protection");
}

on_both_spies!(
    records:
    pins_and_expiries_follow_the_records,
    a_write_adds_o_parents_pin_writes,
    candidates_expire_with_their_lease,
);

// ---------------------------------------------------------------------------
// Claims and deletes.

fn tree_frame(tree: &Tree) -> (ObjectId, Vec<u8>) {
    (
        tree.id().0,
        Frame::encode(ObjectKind::Tree, &tree.encode()).to_bytes(),
    )
}

/// The directory `n` holding one file `f` with `data`: the tree a commit of
/// `n/f` derives.
fn dir_tree(data: &[u8]) -> Tree {
    Tree {
        entries: vec![TreeEntry {
            name: "f".to_owned(),
            kind: EntryKind::File,
            id: blob_id(data),
        }],
    }
}

fn marks<M: MetaStore>(r: &Repo<'_, M>, id: ObjectId) -> Vec<(ObjectId, DoomMark)> {
    block_on(r.e().stores().0.doom_marks(repo_id(r), &[id])).expect("marks")
}

fn holders_of<M: MetaStore>(r: &Repo<'_, M>, id: ObjectId) -> Vec<String> {
    block_on(r.e().stores().0.holder_rows(repo_id(r), id, None, 100)).expect("rows")
}

/// The sweep's delete mark (design §8.6 step 3): only for an object still
/// doomed with no row.
fn delete_mark(object: ObjectId) -> MetaTxn {
    MetaTxn {
        preconditions: vec![
            Precondition::ObjectDoomed(object),
            Precondition::ObjectUnheld(object),
        ],
        writes: vec![Write::MarkDeleting(object)],
    }
}

fn a_doomed_tree_is_claimed_before_the_commit<M: MetaStore>(f: &Fixture<Spy<M>>, spy: &Spy<M>) {
    let (r, root) = seeded(f);
    let tree = dir_tree(b"n1");
    let (id, frame) = tree_frame(&tree);
    // An orphan frame of the tree the commit will derive, doomed long ago.
    block_on(f.objects.put(repo_id(&r), id, frame)).expect("put");
    commit_txn(
        &r,
        vec![Write::Doom {
            object: id,
            since_ms: 0,
        }],
    );

    // Between the claim and the commit, the sweep tries to mark it.
    spy.arm(delete_mark(id));
    r.upload(&[b"n1"]);
    let created = r
        .try_commit(vec![root], vec![put("n/f", b"n1")])
        .expect("commit");
    let hooked = spy.hooked().expect("the hook ran").expect("commit");
    assert_eq!(
        hooked,
        TxnOutcome::PreconditionFailed(0),
        "a claimed tree is not doomed"
    );
    assert!(marks(&r, id).is_empty());
    let holders = holders_of(&r, id);
    assert!(holders.contains(&created.id.to_string()), "{holders:?}");
    assert!(
        holders
            .iter()
            .any(|h| h.starts_with(graft_engine::CLAIM_HOLDER)),
        "{holders:?}"
    );
    check(&r).expect("protection");
}

fn a_tree_doomed_after_the_claim_is_revived_by_the_commit<M: MetaStore>(
    f: &Fixture<Spy<M>>,
    spy: &Spy<M>,
) {
    let (r, root) = seeded(f);
    let (id, _) = tree_frame(&dir_tree(b"n2"));
    // Not doomed when the write reads the marks; doomed just before its
    // commit (it has no row yet, so the doom is allowed).
    spy.arm(MetaTxn {
        preconditions: vec![],
        writes: vec![Write::Doom {
            object: id,
            since_ms: f.clock.now().ms,
        }],
    });
    r.upload(&[b"n2"]);
    let created = r
        .try_commit(vec![root], vec![put("n/f", b"n2")])
        .expect("commit");
    assert!(matches!(spy.hooked(), Some(Ok(TxnOutcome::Committed(_)))));
    assert!(marks(&r, id).is_empty(), "the commit cleared the mark");
    assert_eq!(holders_of(&r, id), [created.id.to_string()]);
}

fn a_named_blob_being_deleted_is_missing<M: MetaStore>(f: &Fixture<Spy<M>>, _spy: &Spy<M>) {
    let (r, root) = seeded(f);
    let blob = r.upload(&[b"gone"])[0];
    let upload = holders_of(&r, blob).pop().expect("upload row");
    commit_txn(
        &r,
        vec![
            Write::RemoveHolder {
                object: blob,
                holder: upload,
            },
            Write::Doom {
                object: blob,
                since_ms: 0,
            },
            Write::MarkDeleting(blob),
        ],
    );
    let got = r.try_commit(vec![root], vec![put("x", b"gone")]);
    assert_eq!(got, Err(ApiError::MissingObjects { ids: vec![blob] }));
    // The upload of the same blob waits for the delete too.
    let put_again = block_on(r.e().put_objects(
        &r.cx,
        PutObjects {
            repo: r.name.clone(),
            blobs: vec![Frame::encode(ObjectKind::Blob, b"gone")],
        },
    ));
    assert!(
        matches!(put_again, Err(ApiError::Unavailable { .. })),
        "{put_again:?}"
    );
    // Once the delete is done, the upload stores it again.
    block_on(f.objects.delete(repo_id(&r), blob)).expect("delete");
    commit_txn(&r, vec![Write::Undoom(blob)]);
    assert_eq!(r.upload(&[b"gone"]), [blob]);
    r.try_commit(vec![root], vec![put("x", b"gone")])
        .expect("commit after re-upload");
    check(&r).expect("protection");
}

fn an_upload_claims_a_doomed_blob<M: MetaStore>(f: &Fixture<Spy<M>>, _spy: &Spy<M>) {
    let (r, _) = seeded(f);
    let blob = r.upload(&[b"old"])[0];
    let upload = holders_of(&r, blob).pop().expect("upload row");
    commit_txn(
        &r,
        vec![
            Write::RemoveHolder {
                object: blob,
                holder: upload,
            },
            Write::Doom {
                object: blob,
                since_ms: 0,
            },
        ],
    );
    r.upload(&[b"old"]);
    assert!(marks(&r, blob).is_empty());
    let holders = holders_of(&r, blob);
    assert!(
        holders
            .iter()
            .any(|h| h.starts_with(graft_engine::CLAIM_HOLDER)),
        "{holders:?}"
    );
    let lease = f.clock.now().ms + config().upload_lease_ms as i64;
    let expiring =
        block_on(r.e().stores().0.expired_rows(repo_id(&r), lease, None, 10)).expect("rows");
    assert!(
        expiring
            .iter()
            .any(|row| row.object == blob && row.holder.starts_with("upload:")),
        "the upload row carries its expiry: {expiring:?}"
    );
}

on_both_spies!(
    claims:
    a_doomed_tree_is_claimed_before_the_commit,
    a_tree_doomed_after_the_claim_is_revived_by_the_commit,
    a_named_blob_being_deleted_is_missing,
    an_upload_claims_a_doomed_blob,
);

/// A sleeper that finishes a delete (frame, then mark) the first time a
/// write waits.
struct FinishDelete<M> {
    meta: Arc<M>,
    objects: Faulty,
    target: Mutex<Option<(RepoId, ObjectId)>>,
    sleeps: AtomicU64,
}

impl<M: MetaStore + 'static> Sleeper for FinishDelete<M> {
    fn sleep(&self, _ms: u64) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        self.sleeps.fetch_add(1, Ordering::SeqCst);
        let target = self.target.lock().expect("target").take();
        let (meta, objects) = (self.meta.clone(), self.objects.clone());
        Box::pin(async move {
            if let Some((repo, object)) = target {
                objects.delete(repo, object).await.expect("delete");
                meta.commit(
                    repo,
                    MetaTxn {
                        preconditions: vec![],
                        writes: vec![Write::Undoom(object)],
                    },
                )
                .await
                .expect("undoom");
            }
        })
    }
}

fn a_derived_tree_being_deleted_is_waited_for_and_put_again<M: MetaStore + 'static>(
    meta: Arc<M>,
    dir: Option<TempDir>,
) {
    let f = build(meta.clone(), config(), dir);
    let objects = f.objects.clone();
    let sleeper = Arc::new(FinishDelete {
        meta: meta.clone(),
        objects: objects.clone(),
        target: Mutex::new(None),
        sleeps: AtomicU64::new(0),
    });
    let f = f.map_engine(|e| e.with_sleeper(sleeper.clone()));
    let (r, root) = seeded(&f);
    let repo = repo_id(&r);
    let (id, frame) = tree_frame(&dir_tree(b"n3"));
    block_on(objects.put(repo, id, frame)).expect("put");
    commit_txn(
        &r,
        vec![
            Write::Doom {
                object: id,
                since_ms: 0,
            },
            Write::MarkDeleting(id),
        ],
    );
    *sleeper.target.lock().expect("target") = Some((repo, id));
    r.upload(&[b"n3"]);
    let created = r
        .try_commit(vec![root], vec![put("n/f", b"n3")])
        .expect("commit");
    assert_eq!(sleeper.sleeps.load(Ordering::SeqCst), 1, "it waited once");
    assert!(
        block_on(objects.get(repo, id)).expect("get").is_some(),
        "the deleted frame was put again"
    );
    assert_eq!(holders_of(&r, id), [created.id.to_string()]);
    assert_eq!(
        r.fetch(created.root).get("n/f").map(Vec::as_slice),
        Some(&b"n3"[..])
    );
    check(&r).expect("protection");
}

#[test]
fn a_derived_tree_being_deleted_is_waited_for_on_memory() {
    a_derived_tree_being_deleted_is_waited_for_and_put_again(Arc::new(MemoryStore::new()), None);
}

#[test]
fn a_derived_tree_being_deleted_is_waited_for_on_sqlite() {
    let dir = TempDir::new("protect-wait");
    let store = SqliteMetaStore::open(SqliteConfig::new(dir_db(&dir))).expect("open");
    a_derived_tree_being_deleted_is_waited_for_and_put_again(Arc::new(store), Some(dir));
}

// ---------------------------------------------------------------------------
// Races on real threads against SQLite.

/// The sweep's collection of `id` (design §8.6 step 1.3), reduced to its
/// precondition and its state change: the record unchanged and unpinned.
fn collect_txn(id: ChangesetId, record: ChangesetRecord, protection: Protection) -> MetaTxn {
    let mut collected = record.clone();
    collected.state = RecordState::Collected;
    MetaTxn {
        preconditions: vec![Precondition::RecordIs {
            id,
            record,
            protection,
        }],
        writes: vec![Write::SetRecord {
            id,
            record: collected,
        }],
    }
}

/// An abandoned parent on `root`, expired and unpinned: collectable. Returns
/// it with the record and protection a collection would compare.
fn collectable<M: MetaStore>(
    r: &Repo<'_, M>,
    root: ChangesetId,
    tag: &str,
) -> (ChangesetId, ChangesetRecord, Protection) {
    let data = format!("p-{tag}").into_bytes();
    r.upload(&[&data, b"child"]);
    let parent = r
        .try_commit(vec![root], vec![put("p", &data)])
        .expect("parent")
        .id;
    let (_, record) = block_on(r.e().stores().0.changeset(repo_id(r), parent))
        .expect("read")
        .expect("stored");
    (parent, record, protection_of(r, parent))
}

/// The two orders, deterministically: a collection that commits between the
/// create's reads and its commit fails the create's precondition; one after
/// the create's commit fails its own.
fn a_create_and_its_parents_collection_never_both_commit<M: MetaStore>(
    f: &Fixture<Spy<M>>,
    spy: &Spy<M>,
) {
    let (r, root) = seeded(f);
    let (parent, record, protection) = collectable(&r, root, "first");
    spy.arm(collect_txn(parent, record, protection));
    let child = r.try_commit(vec![parent], vec![put("q", b"child")]);
    assert_eq!(child, Err(ApiError::Collected { id: parent }));
    assert!(matches!(spy.hooked(), Some(Ok(TxnOutcome::Committed(_)))));

    let (parent, record, protection) = collectable(&r, root, "second");
    r.try_commit(vec![parent], vec![put("q", b"child")])
        .expect("child");
    let late = block_on(
        r.e()
            .stores()
            .0
            .commit(repo_id(&r), collect_txn(parent, record, protection)),
    )
    .expect("commit");
    assert_eq!(late, TxnOutcome::PreconditionFailed(0));
    assert_eq!(protection_of(&r, parent).pins, 1);
}

on_both_spies!(
    collection:
    a_create_and_its_parents_collection_never_both_commit,
);

/// Real threads against SQLite. The collection waits until the create has
/// made `k` record reads (or finished), for `k` from 0 to 3, so it lands
/// anywhere from before the create's first read to after its commit. Either
/// the create pins the parent and the collection fails, or the collection
/// commits and the create fails `Collected`; never both, never neither.
#[test]
fn a_create_racing_its_parents_collection_pins_it_or_fails_collected() {
    let (f, spy) = spy_sqlite();
    let (r, root) = seeded(&f);
    let meta = f.engine.stores().0;
    let repo = repo_id(&r);
    let (mut pinned, mut collected) = (0, 0);
    for i in 0..48 {
        let (parent, record, protection) = collectable(&r, root, &i.to_string());
        let reads = spy.reads.lock().expect("reads").len();
        let wait_for = reads + i % 4;
        let done = std::sync::atomic::AtomicBool::new(false);
        let barrier = Barrier::new(2);
        let (child, collection) = std::thread::scope(|s| {
            let child = s.spawn(|| {
                barrier.wait();
                let got = r.try_commit(vec![parent], vec![put("q", b"child")]);
                done.store(true, Ordering::SeqCst);
                got
            });
            let collection = s.spawn(|| {
                barrier.wait();
                while spy.reads.lock().expect("reads").len() < wait_for
                    && !done.load(Ordering::SeqCst)
                {
                    std::thread::yield_now();
                }
                block_on(meta.commit(repo, collect_txn(parent, record, protection)))
                    .expect("commit")
            });
            (
                child.join().expect("child"),
                collection.join().expect("collection"),
            )
        });
        let (_, after) = block_on(meta.changeset(repo, parent))
            .expect("read")
            .expect("stored");
        match (child, collection) {
            (Ok(_), TxnOutcome::PreconditionFailed(0)) => {
                assert_eq!(after.state, RecordState::Abandoned);
                assert_eq!(protection_of(&r, parent).pins, 1);
                pinned += 1;
            }
            (Err(ApiError::Collected { id }), TxnOutcome::Committed(_)) => {
                assert_eq!(id, parent);
                assert_eq!(after.state, RecordState::Collected);
                collected += 1;
            }
            (child, collection) => {
                panic!("round {i}: child {child:?}, collection {collection:?}")
            }
        }
    }
    assert_eq!(pinned + collected, 48);
    // A collection that waits for the finished create always loses.
    assert!(pinned >= 12, "pinned {pinned}, collected {collected}");
}

#[test]
fn concurrent_children_each_pin_their_parent() {
    let f = sqlite_with(config());
    let (r, root) = seeded(&f);
    r.upload(&[b"parent"]);
    let parent = r
        .try_commit(vec![root], vec![put("p", b"parent")])
        .expect("parent")
        .id;
    let blobs: Vec<Vec<u8>> = (0..16).map(|i| format!("c{i}").into_bytes()).collect();
    let refs: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
    r.upload(&refs);
    let barrier = Barrier::new(blobs.len());
    std::thread::scope(|s| {
        for data in &blobs {
            let (r, barrier) = (&r, &barrier);
            s.spawn(move || {
                barrier.wait();
                r.try_commit(vec![parent], vec![put("q", data)])
                    .expect("child");
            });
        }
    });
    assert_eq!(protection_of(&r, parent).pins, blobs.len() as u32);
    check(&r).expect("protection");
}

// ---------------------------------------------------------------------------
// The backfill.

/// Writes a history the way today's code does, then takes away what the
/// protection records add, as a repository written before them has it.
fn unprotected_history<M: MetaStore>(r: &Repo<'_, M>, root: ChangesetId) {
    open(r, "w").expect("open");
    open(r, "v").expect("open");
    let a = commit_in(r, "w", vec![root], "a", b"a1").expect("a").id;
    let b = commit_in(r, "w", vec![a], "b", b"b1").expect("b").id;
    commit_in(r, "w", vec![b], "d/e/f", b"f1").expect("c");
    let v = commit_in(r, "v", vec![root], "g", b"g1").expect("v").id;
    commit_in(r, "v", vec![v, b], "d/c", b"c1").expect("merge");
    // Abandoned, superseded and landed records.
    r.upload(&[b"x1"]);
    r.try_commit(vec![a], vec![put("x", b"x1")]).expect("x");
    block_on(r.e().sync(&r.cx, wref(r, "w"), None)).expect("sync");
    let w = workspace(r, "w").expect("w").head.expect("head");
    land(r, w).expect("land");
    block_on(r.e().close_workspace(&r.cx, wref(r, "v"), false)).expect("close");
    open(r, "u").expect("open");
    let u = commit_in(r, "u", vec![tip(r)], "a", b"a9").expect("u").id;
    commit_in(r, "u", vec![u], "b", b"b9").expect("u2");
    check(r).expect("protection before the reset");

    let meta = r.e().stores().0;
    let repo = repo_id(r);
    let mut writes = vec![Write::SetGcProtection(false)];
    for s in all_records(meta, repo) {
        if s.record.state != RecordState::Landed {
            writes.push(Write::SetPins { id: s.id, pins: 0 });
            writes.push(Write::SetExpiry {
                id: s.id,
                expires_at_ms: None,
            });
        }
    }
    commit_txn(r, writes);
}

fn landed_reads<M: MetaStore>(r: &Repo<'_, Spy<M>>, spy: &Spy<M>) -> Vec<ChangesetId> {
    let meta = r.e().stores().0;
    let repo = repo_id(r);
    let reads = spy.reads.lock().expect("reads").clone();
    reads
        .into_iter()
        .filter(|id| {
            block_on(meta.changeset(repo, *id))
                .expect("read")
                .is_some_and(|(_, rec)| rec.state == RecordState::Landed)
        })
        .collect()
}

fn the_backfill_restores_protection<M: MetaStore>(f: &Fixture<Spy<M>>, spy: &Spy<M>) {
    let (r, root) = seeded(f);
    unprotected_history(&r, root);
    let repo = repo_id(&r);
    assert!(!block_on(r.e().stores().0.gc_protection(repo)).expect("marker"));

    spy.reads.lock().expect("reads").clear();
    spy.listed.lock().expect("listed").clear();
    let report = block_on(r.e().protect_repo(&r.name, 2)).expect("backfill");
    assert_eq!(report.rows_added, 0, "today's code writes every row");
    assert!(report.pins > 0 && report.expiries > 0, "{report:?}");
    assert_eq!(landed_reads(&r, spy), [], "no landed record is read");
    assert!(
        !spy.listed
            .lock()
            .expect("listed")
            .contains(&RecordState::Landed)
    );
    assert!(block_on(r.e().stores().0.gc_protection(repo)).expect("marker"));
    check(&r).expect("protection after the backfill");

    // Once marked, it is left alone.
    let again = block_on(r.e().protect_repo(&r.name, 2)).expect("again");
    assert_eq!(
        again,
        ProtectReport {
            already: true,
            ..ProtectReport::default()
        }
    );
}

fn the_backfill_adds_a_missing_row<M: MetaStore>(f: &Fixture<Spy<M>>, _spy: &Spy<M>) {
    let (r, root) = seeded(f);
    open(&r, "w").expect("open");
    let a = commit_in(&r, "w", vec![root], "d/e/f", b"deep")
        .expect("a")
        .id;
    let cs = r.changeset(a).changeset;
    let (dir, _) = tree_frame(&dir_tree(b"deep"));
    let dir_e = dir; // the tree holding `f`
    commit_txn(
        &r,
        vec![
            Write::RemoveHolder {
                object: dir_e,
                holder: a.to_string(),
            },
            Write::SetGcProtection(false),
        ],
    );
    assert!(check(&r).is_err(), "a missing row is caught by the check");
    let report = block_on(r.e().protect_repo(&r.name, 10)).expect("backfill");
    assert_eq!(report.rows_added, 1, "{report:?}");
    check(&r).expect("protection after the backfill");
    assert_eq!(r.changeset(a).changeset, cs);
}

on_both_spies!(
    backfill:
    the_backfill_restores_protection,
    the_backfill_adds_a_missing_row,
);

/// The migration from a version 3 database: the repository is written by
/// the engine, copied into a database at schema version 3 (the columns it
/// had), opened (which migrates it to version 4), and backfilled.
#[test]
fn the_backfill_on_a_version_3_database() {
    let source = TempDir::new("protect-v4");
    let objects;
    {
        let store = SqliteMetaStore::open(SqliteConfig::new(dir_db(&source))).expect("open");
        let f = build(Arc::new(store), config(), None);
        let (r, root) = seeded(&f);
        unprotected_history(&r, root);
        objects = f.objects.clone();
    }
    let target = TempDir::new("protect-v3");
    {
        let mut conn = rusqlite_open(&dir_db(&target));
        graft_store_sqlite::migrations::migrate_to(&mut conn, 3).expect("v3");
        conn.execute(
            "ATTACH DATABASE ?1 AS src",
            [dir_db(&source).to_str().expect("utf-8 path")],
        )
        .expect("attach");
        conn.execute_batch(
            "INSERT INTO repos (id, name) SELECT id, name FROM src.repos;
             INSERT INTO changesets
                 SELECT repo, id, content, lsn, received_at, state, holder, superseded_by
                 FROM src.changesets;
             INSERT INTO refs SELECT * FROM src.refs;
             INSERT INTO journal SELECT * FROM src.journal;
             INSERT INTO workspaces
                 SELECT repo, name, owner, session, line, base, head, lease_until
                 FROM src.workspaces;
             INSERT INTO derived SELECT * FROM src.derived;
             INSERT INTO line_pos SELECT * FROM src.line_pos;
             INSERT INTO line_paths SELECT * FROM src.line_paths;
             INSERT INTO holders SELECT repo, object, holder FROM src.holders;
             INSERT INTO lands SELECT * FROM src.lands;
             DETACH DATABASE src;",
        )
        .expect("copy");
    }
    let store = SqliteMetaStore::open(SqliteConfig::new(dir_db(&target))).expect("migrate");
    let meta = Arc::new(Spy::new(store));
    let f = build_over(meta.clone(), objects, config(), Some(target));
    let r = Repo {
        f: &f,
        name: "r".to_owned(),
        cx: admin("r"),
    };
    let repo = repo_id(&r);
    assert!(!block_on(meta.gc_protection(repo)).expect("marker"));
    let report = block_on(f.engine.protect_repo("r", 3)).expect("backfill");
    assert_eq!(report.rows_added, 0, "{report:?}");
    assert!(report.pins > 0, "{report:?}");
    assert_eq!(landed_reads(&r, &meta), []);
    assert!(block_on(meta.gc_protection(repo)).expect("marker"));
    check(&r).expect("protection after the migration");
}

fn rusqlite_open(path: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path).expect("open")
}
