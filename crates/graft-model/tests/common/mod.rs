//! Shared test helpers: an in-memory object source that counts reads, a
//! tiny executor, and a flat `BTreeMap` model of a snapshot.

#![allow(dead_code, unreachable_pub)]

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use graft_model::tree_ops::{Applied, ObjectSource, SourceError};
use graft_model::{BlobId, Change, EntryKind, ObjectId, RepoPath, Tree, TreeId};

/// Runs a future whose sources never wait.
pub fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

/// Trees in memory, with a read counter.
#[derive(Default)]
pub struct MemSource {
    trees: Mutex<HashMap<TreeId, Tree>>,
    reads: AtomicUsize,
}

impl MemSource {
    pub fn write(&self, applied: &Applied) {
        let mut trees = self.trees.lock().unwrap();
        for (id, tree) in &applied.new_trees {
            assert_eq!(*id, tree.id(), "new tree id matches its contents");
            trees.insert(*id, tree.clone());
        }
    }

    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    pub fn reset_reads(&self) {
        self.reads.store(0, Ordering::SeqCst);
    }

    pub fn len(&self) -> usize {
        self.trees.lock().unwrap().len()
    }
}

impl ObjectSource for MemSource {
    async fn tree(&self, id: TreeId) -> Result<Tree, SourceError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.trees
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(SourceError::Missing(id))
    }
}

pub fn path(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

pub fn blob(content: &str) -> ObjectId {
    BlobId::hash(content.as_bytes()).0
}

pub fn put(p: &str, content: &str) -> Change {
    Change::Put {
        path: path(p),
        kind: EntryKind::File,
        id: blob(content),
    }
}

pub fn del(p: &str) -> Change {
    Change::Delete { path: path(p) }
}

/// A flat model of a snapshot: path to (kind, id).
pub type Model = BTreeMap<RepoPath, (EntryKind, ObjectId)>;

/// Builds the snapshot for `model` from empty, writing its trees.
pub fn build(source: &MemSource, model: &Model) -> Option<TreeId> {
    let changes: Vec<Change> = model
        .iter()
        .map(|(p, (kind, id))| Change::Put {
            path: p.clone(),
            kind: *kind,
            id: *id,
        })
        .collect();
    let applied = block_on(graft_model::apply_changes(source, None, &changes)).unwrap();
    source.write(&applied);
    applied.root
}

/// Walks a whole snapshot into the flat model (tests only).
pub fn flatten(source: &MemSource, root: Option<TreeId>) -> Model {
    let mut out = Model::new();
    if let Some(root) = root {
        walk(source, root, None, &mut out);
    }
    out
}

fn walk(source: &MemSource, id: TreeId, dir: Option<&RepoPath>, out: &mut Model) {
    let tree = block_on(source.tree(id)).unwrap();
    assert!(!tree.is_empty(), "only an empty root may be an empty tree");
    for e in &tree.entries {
        let p = match dir {
            None => path(&e.name),
            Some(d) => d.join(&e.name).unwrap(),
        };
        if e.kind == EntryKind::Tree {
            walk(source, TreeId(e.id), Some(&p), out);
        } else {
            out.insert(p, (e.kind, e.id));
        }
    }
}

/// The changes that turn `from` into `to`, from the flat models, sorted by
/// path bytes.
pub fn model_diff(from: &Model, to: &Model) -> Vec<Change> {
    let mut out = Vec::new();
    for p in from.keys() {
        if !to.contains_key(p) {
            out.push(Change::Delete { path: p.clone() });
        }
    }
    for (p, &(kind, id)) in to {
        if from.get(p) != Some(&(kind, id)) {
            out.push(Change::Put {
                path: p.clone(),
                kind,
                id,
            });
        }
    }
    out.sort_by(|a, b| {
        a.path()
            .as_str()
            .as_bytes()
            .cmp(b.path().as_str().as_bytes())
    });
    out
}
