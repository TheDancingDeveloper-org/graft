//! Helpers shared by the integration tests.

#![allow(dead_code, unreachable_pub)]

use std::collections::BTreeMap;
use std::future::{Future, ready};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;

use graft_model::ObjectId;
use graft_store::{ObjectStore, RepoId, StoreResult, StoreStats};

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

/// A directory removed when dropped.
#[derive(Debug)]
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("graft-sqlite-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn db(&self) -> PathBuf {
        self.0.join("meta.sqlite")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

type Frames = BTreeMap<(RepoId, ObjectId), Vec<u8>>;

/// A trivial object store for the conformance suite's object cases, which
/// this crate does not implement. It accepts any repository id.
#[derive(Clone, Debug, Default)]
pub struct Objects(Arc<Mutex<Frames>>);

impl ObjectStore for Objects {
    fn put(
        &self,
        repo: RepoId,
        id: ObjectId,
        frame: Vec<u8>,
    ) -> impl Future<Output = StoreResult<()>> + Send {
        self.0.lock().unwrap().entry((repo, id)).or_insert(frame);
        ready(Ok(()))
    }

    fn get(
        &self,
        repo: RepoId,
        id: ObjectId,
    ) -> impl Future<Output = StoreResult<Option<Vec<u8>>>> + Send {
        ready(Ok(self.0.lock().unwrap().get(&(repo, id)).cloned()))
    }

    fn has(
        &self,
        repo: RepoId,
        ids: &[ObjectId],
    ) -> impl Future<Output = StoreResult<Vec<ObjectId>>> + Send {
        let map = self.0.lock().unwrap();
        ready(Ok(ids
            .iter()
            .copied()
            .filter(|id| map.contains_key(&(repo, *id)))
            .collect()))
    }

    fn delete(&self, repo: RepoId, id: ObjectId) -> impl Future<Output = StoreResult<()>> + Send {
        self.0.lock().unwrap().remove(&(repo, id));
        ready(Ok(()))
    }

    fn stats(&self) -> impl Future<Output = StoreResult<StoreStats>> + Send {
        let map = self.0.lock().unwrap();
        ready(Ok(StoreStats {
            objects: map.len() as u64,
            bytes: map.values().map(|f| f.len() as u64).sum(),
            free_bytes: u64::MAX,
        }))
    }
}
