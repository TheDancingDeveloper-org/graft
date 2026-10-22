//! Shared fixtures: an executor, engines over the memory and SQLite
//! stores, a failure-injecting object store, and request builders.

#![allow(dead_code, unreachable_pub)]

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;

use graft_engine::{Engine, EngineConfig};
use graft_model::{
    Author, AuthorKind, Change, ChangesetId, EntryKind, Metadata, ObjectId, ObjectKind, RepoPath,
    Timestamp, Tree, TreeId,
};
use graft_proto::error::ApiResult;
use graft_proto::types::*;
use graft_proto::{AdminApi, CallContext, Frame, ObjectApi, Principal, PrincipalKind, RepoApi};
use graft_store::memory::MemoryStore;
use graft_store::{MetaStore, ObjectStore, RepoId, StoreError, StoreResult, StoreStats};
use graft_store_sqlite::{SqliteConfig, SqliteMetaStore};
use graft_testkit::clock::FakeClock;

struct Unpark(thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Drives a future to completion on this thread, parking between polls.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        thread::park();
    }
}

/// Frames by repository and id.
type Frames = BTreeMap<(RepoId, ObjectId), Vec<u8>>;

/// A standalone in-memory object store that can be told to fail. (The
/// memory store's object half only knows repositories created through its
/// own metadata half, so it cannot pair with the SQLite store.)
#[derive(Clone, Debug, Default)]
pub struct Faulty {
    pub frames: Arc<Mutex<Frames>>,
    pub fail: Arc<AtomicBool>,
    /// How many `get` calls it has answered.
    pub gets: Arc<AtomicU64>,
}

impl Faulty {
    fn check(&self) -> StoreResult<()> {
        if self.fail.load(Ordering::SeqCst) {
            Err(StoreError::Busy(7))
        } else {
            Ok(())
        }
    }

    fn map(&self) -> std::sync::MutexGuard<'_, Frames> {
        self.frames.lock().expect("frames")
    }

    /// Overwrites a stored frame, bypassing put-if-absent.
    pub fn corrupt(&self, repo: RepoId, id: ObjectId, bytes: Vec<u8>) {
        self.map().insert((repo, id), bytes);
    }
}

impl ObjectStore for Faulty {
    async fn put(&self, repo: RepoId, id: ObjectId, frame: Vec<u8>) -> StoreResult<()> {
        self.check()?;
        self.map().entry((repo, id)).or_insert(frame);
        Ok(())
    }
    async fn get(&self, repo: RepoId, id: ObjectId) -> StoreResult<Option<Vec<u8>>> {
        self.check()?;
        self.gets.fetch_add(1, Ordering::SeqCst);
        Ok(self.map().get(&(repo, id)).cloned())
    }
    async fn has(&self, repo: RepoId, ids: &[ObjectId]) -> StoreResult<Vec<ObjectId>> {
        self.check()?;
        let map = self.map();
        Ok(ids
            .iter()
            .copied()
            .filter(|id| map.contains_key(&(repo, *id)))
            .collect())
    }
    async fn delete(&self, repo: RepoId, id: ObjectId) -> StoreResult<()> {
        self.map().remove(&(repo, id));
        Ok(())
    }
    async fn stats(&self) -> StoreResult<StoreStats> {
        let map = self.map();
        Ok(StoreStats {
            objects: map.len() as u64,
            bytes: map.values().map(|f| f.len() as u64).sum(),
            free_bytes: u64::MAX,
        })
    }
}

/// A directory removed when dropped.
#[derive(Debug)]
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("graft-engine-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An engine and what it was built over.
pub struct Fixture<M> {
    pub engine: Engine<M, Faulty>,
    pub objects: Faulty,
    pub clock: Arc<FakeClock>,
    _dir: Option<TempDir>,
}

pub fn memory() -> Fixture<MemoryStore> {
    memory_with(EngineConfig::default())
}

pub fn memory_with(config: EngineConfig) -> Fixture<MemoryStore> {
    build(Arc::new(MemoryStore::new()), config, None)
}

pub fn sqlite() -> Fixture<SqliteMetaStore> {
    sqlite_with(EngineConfig::default())
}

pub fn sqlite_with(config: EngineConfig) -> Fixture<SqliteMetaStore> {
    let dir = TempDir::new("sqlite");
    let meta = SqliteMetaStore::open(SqliteConfig::new(dir.0.join("meta.sqlite")))
        .expect("open sqlite store");
    build(Arc::new(meta), config, Some(dir))
}

pub fn build<M: MetaStore>(meta: Arc<M>, config: EngineConfig, dir: Option<TempDir>) -> Fixture<M> {
    let objects = Faulty::default();
    let clock = Arc::new(FakeClock::at(1_000_000));
    let engine = Engine::new(meta, Arc::new(objects.clone()), clock.clone(), config);
    Fixture {
        engine,
        objects,
        clock,
        _dir: dir,
    }
}

/// Generates one `#[test]` per store for each generic scenario
/// `fn name<M: MetaStore>(f: &Fixture<M>)`.
#[macro_export]
macro_rules! on_both_stores {
    ($($name:ident),* $(,)?) => {
        mod memory_store {
            $( #[test] fn $name() { super::$name(&$crate::common::memory()) } )*
        }
        mod sqlite_store {
            $( #[test] fn $name() { super::$name(&$crate::common::sqlite()) } )*
        }
    };
}

pub fn cx(scopes: &[&str]) -> CallContext {
    CallContext {
        principal: Principal {
            subject: "tester".to_owned(),
            kind: PrincipalKind::Human,
            session: None,
            work_item: None,
            scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
        },
        request_id: "req-1".to_owned(),
        deadline_ms: None,
    }
}

pub fn admin(repo: &str) -> CallContext {
    cx(&[&format!("repo:{repo}:admin")])
}

pub fn p(s: &str) -> RepoPath {
    RepoPath::new(s).expect("valid path")
}

pub fn blob_id(data: &[u8]) -> ObjectId {
    ObjectId::hash(ObjectKind::Blob, data)
}

pub fn put(path: &str, data: &[u8]) -> Change {
    Change::Put {
        path: p(path),
        kind: EntryKind::File,
        id: blob_id(data),
    }
}

pub fn del(path: &str) -> Change {
    Change::Delete { path: p(path) }
}

pub fn author() -> Author {
    Author {
        name: "Tess".to_owned(),
        email: "tess@example.com".to_owned(),
        kind: AuthorKind::Agent,
    }
}

pub fn commit_req(repo: &str, parents: Vec<ChangesetId>, changes: Vec<Change>) -> CreateChangeset {
    CreateChangeset {
        repo: repo.to_owned(),
        parents,
        changes,
        author: author(),
        authored_at: Timestamp {
            ms: 1_700_000_000_000,
            offset_min: 60,
        },
        message: "change".to_owned(),
        metadata: Metadata::new(),
        inline_blobs: Vec::new(),
        workspace: None,
    }
}

/// A repository with an admin caller, and shortcuts for common calls.
pub struct Repo<'f, M> {
    pub f: &'f Fixture<M>,
    pub name: String,
    pub cx: CallContext,
}

impl<'f, M: MetaStore> Repo<'f, M> {
    pub fn new(f: &'f Fixture<M>, name: &str) -> Self {
        let cx = admin(name);
        block_on(f.engine.create_repo(&cx, name.to_owned())).expect("create repo");
        Self {
            f,
            name: name.to_owned(),
            cx,
        }
    }

    pub fn e(&self) -> &Engine<M, Faulty> {
        &self.f.engine
    }

    /// Uploads blobs, returning their ids.
    pub fn upload(&self, blobs: &[&[u8]]) -> Vec<ObjectId> {
        let frames = blobs
            .iter()
            .map(|b| Frame::encode(ObjectKind::Blob, b))
            .collect();
        block_on(self.e().put_objects(
            &self.cx,
            PutObjects {
                repo: self.name.clone(),
                blobs: frames,
            },
        ))
        .expect("upload")
    }

    pub fn try_commit(
        &self,
        parents: Vec<ChangesetId>,
        changes: Vec<Change>,
    ) -> ApiResult<Created> {
        block_on(
            self.e()
                .create_changeset(&self.cx, commit_req(&self.name, parents, changes)),
        )
    }

    /// Uploads the contents and commits `files` (path, contents) and
    /// `deletes` on `parents`.
    pub fn commit(
        &self,
        parents: Vec<ChangesetId>,
        files: &[(&str, &[u8])],
        deletes: &[&str],
    ) -> Created {
        let blobs: Vec<&[u8]> = files.iter().map(|(_, b)| *b).collect();
        if !blobs.is_empty() {
            self.upload(&blobs);
        }
        let mut changes: Vec<Change> = files.iter().map(|(path, b)| put(path, b)).collect();
        changes.extend(deletes.iter().map(|path| del(path)));
        changes.sort_by(|a, b| a.path().cmp(b.path()));
        self.try_commit(parents, changes).expect("commit")
    }

    pub fn update_ref(
        &self,
        name: &str,
        expected: Option<ChangesetId>,
        new: ChangesetId,
        kind: RefKind,
    ) -> ApiResult<RefView> {
        block_on(self.e().update_ref(
            &self.cx,
            UpdateRef {
                repo: self.name.clone(),
                name: name.to_owned(),
                expected,
                new,
                kind,
            },
        ))
    }

    pub fn changeset(&self, id: ChangesetId) -> ChangesetView {
        let mut got = block_on(self.e().get_changesets(
            &self.cx,
            GetChangesets {
                repo: self.name.clone(),
                ids: vec![id],
            },
        ))
        .expect("get changesets");
        got.found.pop().expect("found")
    }

    pub fn journal(&self) -> Vec<JournalEntry> {
        let mut out = Vec::new();
        let mut from = 1;
        loop {
            let page = block_on(self.e().watch(
                &self.cx,
                Watch {
                    repo: self.name.clone(),
                    from_lsn: from,
                    kinds: Vec::new(),
                    limit: 3,
                    wait_ms: 0,
                },
            ))
            .expect("watch");
            if page.entries.is_empty() {
                return out;
            }
            from = page.next_lsn;
            out.extend(page.entries);
        }
    }

    /// Fetches a whole snapshot the way a client does: the root tree, then
    /// each level of subtrees, then the blobs, by `GetObjects` only.
    pub fn fetch(&self, root: TreeId) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut level: VecDeque<(String, ObjectId)> = VecDeque::from([(String::new(), root.0)]);
        while !level.is_empty() {
            let ids: Vec<ObjectId> = level.iter().map(|(_, id)| *id).collect();
            let got = self.get_objects(ids);
            assert!(got.missing.is_empty() && got.deferred.is_empty());
            let mut next = VecDeque::new();
            let mut blobs = Vec::new();
            for ((dir, id), (got_id, frame)) in level.drain(..).zip(got.found) {
                assert_eq!(id, got_id);
                let tree =
                    Tree::decode(&frame.decode_verified(id).expect("verified")).expect("tree");
                for e in tree.entries {
                    let path = if dir.is_empty() {
                        e.name.clone()
                    } else {
                        format!("{dir}/{}", e.name)
                    };
                    if e.kind.is_tree() {
                        next.push_back((path, e.id));
                    } else {
                        blobs.push((path, e.id));
                    }
                }
            }
            for chunk in blobs.chunks(50) {
                let got = self.get_objects(chunk.iter().map(|(_, id)| *id).collect());
                assert!(got.missing.is_empty());
                let by_id: BTreeMap<ObjectId, Frame> = got.found.into_iter().collect();
                for (path, id) in chunk {
                    let data = by_id[id].decode_verified(*id).expect("verified blob");
                    out.insert(path.clone(), data);
                }
            }
            level = next;
        }
        out
    }

    pub fn get_objects(&self, ids: Vec<ObjectId>) -> Objects {
        block_on(self.e().get_objects(
            &self.cx,
            GetObjects {
                repo: self.name.clone(),
                ids,
            },
        ))
        .expect("get objects")
    }
}

/// The changes that turn snapshot `old` into `new`, sorted by path.
pub fn changes_between(
    old: &BTreeMap<String, Vec<u8>>,
    new: &BTreeMap<String, Vec<u8>>,
) -> Vec<Change> {
    let mut out = Vec::new();
    for (path, data) in new {
        if old.get(path) != Some(data) {
            out.push(put(path, data));
        }
    }
    for path in old.keys() {
        if !new.contains_key(path) {
            out.push(del(path));
        }
    }
    out.sort_by(|a, b| a.path().cmp(b.path()));
    out
}
