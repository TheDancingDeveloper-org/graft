//! Shared fixtures: a read-counting `MetaStore` over the memory store, a
//! poll-to-completion executor, and changeset builders.

#![allow(dead_code, unreachable_pub)]

use std::collections::HashMap;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use graft_index::Index;
use graft_model::{
    Author, AuthorKind, Change, Changeset, ChangesetId, EntryKind, ObjectId, ObjectKind, RepoPath,
    Timestamp, TreeId,
};
use graft_proto::Frame;
use graft_store::memory::MemoryStore;
use graft_store::{
    ChangesetRecord, Lsn, MetaStore, MetaTxn, ObjectStore, Precondition, RecordState, RefKind,
    RefRecord, RepoId, StoreResult, TxnOutcome, WorkspaceRecord, Write,
};

/// Runs a future that never waits (the memory store's are ready at once).
pub fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

/// Read counters, by port method.
#[derive(Debug, Default)]
pub struct Reads {
    pub changeset: AtomicUsize,
    pub derived: AtomicUsize,
    pub line_pos: AtomicUsize,
    /// Rows returned by `scan_line_path` (calls count as `other`).
    pub scan_rows: AtomicUsize,
    pub other: AtomicUsize,
}

impl Reads {
    pub fn changeset(&self) -> usize {
        self.changeset.load(Ordering::SeqCst)
    }
    pub fn derived(&self) -> usize {
        self.derived.load(Ordering::SeqCst)
    }
    pub fn line_pos(&self) -> usize {
        self.line_pos.load(Ordering::SeqCst)
    }
    pub fn scan_rows(&self) -> usize {
        self.scan_rows.load(Ordering::SeqCst)
    }
    pub fn other(&self) -> usize {
        self.other.load(Ordering::SeqCst)
    }
    pub fn reset(&self) {
        for c in [
            &self.changeset,
            &self.derived,
            &self.line_pos,
            &self.scan_rows,
            &self.other,
        ] {
            c.store(0, Ordering::SeqCst);
        }
    }
}

fn bump(c: &AtomicUsize) {
    c.fetch_add(1, Ordering::SeqCst);
}

/// A `MetaStore` that counts reads and passes everything through.
#[derive(Clone, Debug)]
pub struct Counting {
    pub inner: MemoryStore,
    pub reads: Arc<Reads>,
}

impl MetaStore for Counting {
    fn create_repo(&self, name: &str) -> impl Future<Output = StoreResult<RepoId>> + Send {
        self.inner.create_repo(name)
    }
    fn repo(&self, name: &str) -> impl Future<Output = StoreResult<Option<RepoId>>> + Send {
        bump(&self.reads.other);
        self.inner.repo(name)
    }
    fn commit(
        &self,
        repo: RepoId,
        txn: MetaTxn,
    ) -> impl Future<Output = StoreResult<TxnOutcome>> + Send {
        self.inner.commit(repo, txn)
    }
    fn changeset(
        &self,
        repo: RepoId,
        id: ChangesetId,
    ) -> impl Future<Output = StoreResult<Option<(Vec<u8>, ChangesetRecord)>>> + Send {
        bump(&self.reads.changeset);
        self.inner.changeset(repo, id)
    }
    fn get_ref(
        &self,
        repo: RepoId,
        name: &str,
    ) -> impl Future<Output = StoreResult<Option<RefRecord>>> + Send {
        bump(&self.reads.other);
        self.inner.get_ref(repo, name)
    }
    fn list_refs(
        &self,
        repo: RepoId,
        prefix: &str,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<RefRecord>>> + Send {
        bump(&self.reads.other);
        self.inner.list_refs(repo, prefix, limit)
    }
    fn workspace(
        &self,
        repo: RepoId,
        name: &str,
    ) -> impl Future<Output = StoreResult<Option<WorkspaceRecord>>> + Send {
        bump(&self.reads.other);
        self.inner.workspace(repo, name)
    }
    fn expired_workspaces(
        &self,
        repo: RepoId,
        before_ms: i64,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send {
        bump(&self.reads.other);
        self.inner.expired_workspaces(repo, before_ms, limit)
    }
    fn list_workspaces(
        &self,
        repo: RepoId,
        owner: &str,
        after: &str,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<WorkspaceRecord>>> + Send {
        bump(&self.reads.other);
        self.inner.list_workspaces(repo, owner, after, limit)
    }
    fn count_records(
        &self,
        repo: RepoId,
        state: graft_store::RecordState,
    ) -> impl Future<Output = StoreResult<u64>> + Send {
        bump(&self.reads.other);
        self.inner.count_records(repo, state)
    }
    fn count_workspaces(
        &self,
        repo: RepoId,
        before_ms: Option<i64>,
    ) -> impl Future<Output = StoreResult<u64>> + Send {
        bump(&self.reads.other);
        self.inner.count_workspaces(repo, before_ms)
    }
    fn derived(
        &self,
        repo: RepoId,
        kind: &str,
        version: u32,
        key: &[u8],
    ) -> impl Future<Output = StoreResult<Option<Vec<u8>>>> + Send {
        bump(&self.reads.derived);
        self.inner.derived(repo, kind, version, key)
    }
    fn line_pos(
        &self,
        repo: RepoId,
        line: &str,
        id: ChangesetId,
    ) -> impl Future<Output = StoreResult<Option<u64>>> + Send {
        bump(&self.reads.line_pos);
        self.inner.line_pos(repo, line, id)
    }
    fn scan_line_path(
        &self,
        repo: RepoId,
        line: &str,
        path: &RepoPath,
        before: u64,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(u64, ChangesetId)>>> + Send {
        bump(&self.reads.other);
        let reads = self.reads.clone();
        let scan = self.inner.scan_line_path(repo, line, path, before, limit);
        async move {
            let rows = scan.await;
            if let Ok(rows) = &rows {
                reads.scan_rows.fetch_add(rows.len(), Ordering::SeqCst);
            }
            rows
        }
    }
    fn holders(
        &self,
        repo: RepoId,
        object: ObjectId,
    ) -> impl Future<Output = StoreResult<u32>> + Send {
        bump(&self.reads.other);
        self.inner.holders(repo, object)
    }
    fn land(
        &self,
        repo: RepoId,
        draft: ChangesetId,
        line: &str,
    ) -> impl Future<Output = StoreResult<Option<ChangesetId>>> + Send {
        bump(&self.reads.other);
        self.inner.land(repo, draft, line)
    }
    fn journal(
        &self,
        repo: RepoId,
        from: Lsn,
        limit: u32,
    ) -> impl Future<Output = StoreResult<Vec<(Lsn, String, Vec<u8>)>>> + Send {
        bump(&self.reads.other);
        self.inner.journal(repo, from, limit)
    }
    fn journal_tip(&self, repo: RepoId) -> impl Future<Output = StoreResult<Option<Lsn>>> + Send {
        bump(&self.reads.other);
        self.inner.journal_tip(repo)
    }
}

/// An index over a fresh memory store with one repository.
pub struct Fixture {
    pub index: Index<Counting, MemoryStore>,
    pub reads: Arc<Reads>,
    pub repo: RepoId,
}

impl Fixture {
    pub fn new() -> Self {
        let store = MemoryStore::new();
        let reads = Arc::new(Reads::default());
        let meta = Counting {
            inner: store.clone(),
            reads: reads.clone(),
        };
        let repo = block_on(meta.create_repo("r")).unwrap();
        Fixture {
            index: Index::new(meta, store),
            reads,
            repo,
        }
    }

    /// Commits writes in one transaction.
    pub fn commit(&self, writes: Vec<Write>) {
        let txn = MetaTxn {
            preconditions: Vec::new(),
            writes,
        };
        let outcome = block_on(self.index.meta().commit(self.repo, txn)).unwrap();
        assert!(matches!(outcome, TxnOutcome::Committed(_)));
    }

    /// Stores a changeset's content and record, with no derived rows.
    pub fn store_only(&self, cs: &Changeset) -> ChangesetId {
        let id = cs.id();
        self.commit(vec![put_changeset(cs)]);
        id
    }

    /// Stores a changeset with its eager rows, as `CreateChangeset` would.
    pub fn create(&self, cs: &Changeset) -> ChangesetId {
        let mut writes = vec![put_changeset(cs)];
        writes.extend(block_on(self.index.on_create(self.repo, cs)).unwrap());
        self.commit(writes);
        cs.id()
    }

    /// Commits the line rows for stored changesets landing on `line`
    /// (oldest first, as `Index::on_land` takes them).
    pub fn land(&self, line: &str, batch: &[Changeset]) {
        let rows = block_on(self.index.on_land(self.repo, line, batch)).unwrap();
        self.commit(rows);
    }

    /// Creates or moves the line ref `line` to `target`.
    pub fn set_line(&self, line: &str, target: ChangesetId) {
        let current = block_on(self.index.meta().get_ref(self.repo, line)).unwrap();
        let expected = current.as_ref().map(|r| (r.target, r.version));
        let version = current.map_or(1, |r| r.version + 1);
        let txn = MetaTxn {
            preconditions: vec![Precondition::RefIs {
                name: line.to_owned(),
                expected,
            }],
            writes: vec![Write::SetRef(RefRecord {
                name: line.to_owned(),
                target,
                version,
                kind: RefKind::Line,
            })],
        };
        let outcome = block_on(self.index.meta().commit(self.repo, txn)).unwrap();
        assert!(matches!(outcome, TxnOutcome::Committed(_)));
    }

    /// Stores `content` as a blob and returns its id.
    pub fn blob(&self, content: &[u8]) -> ObjectId {
        let id = ObjectId::hash(ObjectKind::Blob, content);
        let frame = Frame::encode(ObjectKind::Blob, content).to_bytes();
        block_on(self.index.objects().put(self.repo, id, frame)).unwrap();
        id
    }
}

/// The first-parent chain ending at `tip`, oldest first.
pub fn first_parent_chain(
    css: &HashMap<ChangesetId, Changeset>,
    tip: ChangesetId,
) -> Vec<Changeset> {
    let mut chain = Vec::new();
    let mut cur = Some(tip);
    while let Some(id) = cur {
        let cs = css[&id].clone();
        cur = cs.first_parent();
        chain.push(cs);
    }
    chain.reverse();
    chain
}

pub fn put_changeset(cs: &Changeset) -> Write {
    Write::PutChangeset {
        id: cs.id(),
        content: cs.encode(),
        record: ChangesetRecord {
            lsn: 0,
            received_at_ms: 0,
            state: RecordState::Draft,
            holder: None,
            superseded_by: None,
        },
    }
}

pub fn path(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

pub fn put(p: &str) -> Change {
    put_blob(p, ObjectId([7; 32]))
}

pub fn put_blob(p: &str, id: ObjectId) -> Change {
    Change::Put {
        path: path(p),
        kind: EntryKind::File,
        id,
    }
}

pub fn delete(p: &str) -> Change {
    Change::Delete { path: path(p) }
}

/// A changeset; `tag` keeps ids distinct. Changes are sorted here.
pub fn changeset(parents: Vec<ChangesetId>, tag: u64, mut changes: Vec<Change>) -> Changeset {
    changes.sort_by(|a, b| a.path().cmp(b.path()));
    changes.dedup_by(|a, b| a.path() == b.path());
    Changeset {
        parents,
        root: TreeId(ObjectId([0; 32])),
        changes,
        author: Author {
            name: "t".into(),
            email: "t@example.com".into(),
            kind: AuthorKind::Agent,
        },
        authored_at: Timestamp {
            ms: 0,
            offset_min: 0,
        },
        message: format!("c{tag}"),
        metadata: Default::default(),
    }
}
